//! Delay before the next `try` attempt.
//!
//! The walk does not sleep. It returns the wait on [`crate::Pause::Retry`].
//! A runner keeps the instant it first saw that pause and adds the delay.
//!
//! `attempt` is the zero-based try that just failed.
//!
//! - No `backoff`, or `backoff.constant`: `delay` every time.
//! - `backoff.linear`: `delay + increment * attempt`. A missing `increment` defaults to `delay`, so
//!   the waits are `delay`, `2 * delay`, `3 * delay`, ...
//! - `backoff.exponential`: `delay * 2^attempt`, with the exponent capped at 32. The first retry
//!   waits `delay`, then the wait doubles.
//! - `jitter`: add a point between `from` and `to` after that backoff. [`crate::walk`] uses `from`.
//!   [`crate::walk_with`] can select another point.
//!
//! A missing `delay` is 0. Years in an ISO 8601 duration are 365 days. Months
//! are 30 days. Fractions round to the nearest millisecond, half up.

use std::time::Duration;

use serde_json::Map;
use serde_json::Value;

const MS_SECOND: u128 = 1_000;
const MS_MINUTE: u128 = 60 * MS_SECOND;
const MS_HOUR: u128 = 60 * MS_MINUTE;
const MS_DAY: u128 = 24 * MS_HOUR;
const MS_WEEK: u128 = 7 * MS_DAY;
const MS_MONTH: u128 = 30 * MS_DAY;
const MS_YEAR: u128 = 365 * MS_DAY;
const MAX_EXPONENT: u32 = 32;

/// Milliseconds since the Unix epoch.
///
/// A runner passes the instant it first observed a retry pause. This crate
/// does not store that instant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp {
    millis: u64,
}

impl Timestamp {
    /// `millis` is milliseconds since the Unix epoch.
    #[must_use]
    pub fn from_millis(millis: u64) -> Self {
        Self { millis }
    }

    /// Milliseconds since the Unix epoch.
    #[must_use]
    pub fn millis(self) -> u64 {
        self.millis
    }

    pub(crate) fn saturating_add(self, delay: Duration) -> Self {
        let extra = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX);
        Self {
            millis: self.millis.saturating_add(extra),
        }
    }
}

/// One point in a retry jitter range, in parts per million.
///
/// `0` is `jitter.from`. [`JitterSample::TO`] is `jitter.to`. [`crate::walk`]
/// uses [`JitterSample::FROM`], so a later walk with the same inputs returns
/// the same delay.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct JitterSample(u32);

impl JitterSample {
    /// The `jitter.from` end of the range.
    pub const FROM: Self = Self(0);

    /// The `jitter.to` end of the range. One million parts per million.
    pub const TO: Self = Self(1_000_000);

    /// `parts_per_million` must be `0..=1_000_000`.
    #[must_use]
    pub fn new(parts_per_million: u32) -> Option<Self> {
        if parts_per_million <= 1_000_000 {
            Some(Self(parts_per_million))
        } else {
            None
        }
    }

    /// Parts per million from `from` toward `to`.
    #[must_use]
    pub fn parts_per_million(self) -> u32 {
        self.0
    }
}

pub(crate) enum BackoffKind {
    Constant,
    Linear,
    Exponential,
}

pub(crate) struct RetrySpec<'a> {
    pub delay: Option<&'a Value>,
    pub kind: BackoffKind,
    pub increment: Option<&'a Value>,
    pub jitter_from: Option<&'a Value>,
    pub jitter_to: Option<&'a Value>,
}

pub(crate) fn retry_spec(policy: &Value) -> Result<RetrySpec<'_>, String> {
    let Some(policy) = policy.as_object() else {
        return Err("retry policy must be an object".to_string());
    };
    let (kind, increment) = backoff(policy.get("backoff"))?;
    let (jitter_from, jitter_to) = jitter_bounds(policy.get("jitter"))?;
    Ok(RetrySpec {
        delay: policy.get("delay"),
        kind,
        increment,
        jitter_from,
        jitter_to,
    })
}

pub(crate) fn wait_ms(
    delay_ms: u64,
    kind: BackoffKind,
    increment_ms: Option<u64>,
    jitter_ms: Option<(u64, u64)>,
    attempt: u64,
    sample: JitterSample,
) -> Result<u64, String> {
    let backoff_ms = match kind {
        BackoffKind::Constant => delay_ms,
        BackoffKind::Linear => {
            let increment = increment_ms.unwrap_or(delay_ms);
            saturating_add(delay_ms, saturating_mul(increment, attempt))
        }
        BackoffKind::Exponential => {
            let exponent = u32::try_from(attempt)
                .unwrap_or(MAX_EXPONENT)
                .min(MAX_EXPONENT);
            saturating_mul(delay_ms, 1_u64 << exponent)
        }
    };
    let Some((from_ms, to_ms)) = jitter_ms else {
        return Ok(backoff_ms);
    };
    if from_ms > to_ms {
        return Err("jitter.from must be less than or equal to jitter.to".to_string());
    }
    let span = u128::from(to_ms - from_ms);
    let extra = span * u128::from(sample.parts_per_million()) / 1_000_000;
    Ok(saturating_add(
        backoff_ms,
        saturating_u128(u128::from(from_ms) + extra),
    ))
}

pub(crate) fn duration_millis(value: &Value) -> Result<u64, String> {
    match value {
        Value::String(text) => parse_iso_duration(text),
        Value::Object(object) => parse_object_duration(object),
        _ => Err("duration must be an ISO 8601 string or an object".to_string()),
    }
}

fn backoff(value: Option<&Value>) -> Result<(BackoffKind, Option<&Value>), String> {
    let Some(value) = value else {
        return Ok((BackoffKind::Constant, None));
    };
    let Some(object) = value.as_object() else {
        return Err("backoff must be an object".to_string());
    };
    if object.is_empty() {
        return Err("backoff must set constant, linear, or exponential".to_string());
    }
    let mut selected: Option<(&str, BackoffKind)> = None;
    for key in object.keys() {
        let kind = match key.as_str() {
            "constant" => BackoffKind::Constant,
            "linear" => BackoffKind::Linear,
            "exponential" => BackoffKind::Exponential,
            other => return Err(format!("unknown backoff `{other}`")),
        };
        if selected.is_some() {
            return Err(
                "backoff must set only one of constant, linear, or exponential".to_string(),
            );
        }
        selected = Some((key.as_str(), kind));
    }
    let Some((name, kind)) = selected else {
        return Err("backoff must set constant, linear, or exponential".to_string());
    };
    let Some(body) = object.get(name).and_then(Value::as_object) else {
        return Err(format!("{name} backoff must be an object"));
    };
    let increment = match kind {
        BackoffKind::Linear => {
            for key in body.keys() {
                if key != "increment" {
                    return Err(format!("unknown linear backoff field `{key}`"));
                }
            }
            body.get("increment")
        }
        BackoffKind::Constant | BackoffKind::Exponential => {
            if let Some(key) = body.keys().next() {
                return Err(format!("unknown {name} backoff field `{key}`"));
            }
            None
        }
    };
    Ok((kind, increment))
}

fn jitter_bounds(value: Option<&Value>) -> Result<(Option<&Value>, Option<&Value>), String> {
    let Some(value) = value else {
        return Ok((None, None));
    };
    let Some(object) = value.as_object() else {
        return Err("jitter must be an object".to_string());
    };
    for key in object.keys() {
        if key != "from" && key != "to" {
            return Err(format!("unknown jitter field `{key}`"));
        }
    }
    match (object.get("from"), object.get("to")) {
        (Some(from), Some(to)) => Ok((Some(from), Some(to))),
        _ => Err("jitter requires from and to".to_string()),
    }
}

fn parse_object_duration(object: &Map<String, Value>) -> Result<u64, String> {
    if object.is_empty() {
        return Err("duration object must set at least one component".to_string());
    }
    let mut total: u128 = 0;
    for (name, scale) in [
        ("days", MS_DAY),
        ("hours", MS_HOUR),
        ("minutes", MS_MINUTE),
        ("seconds", MS_SECOND),
        ("milliseconds", 1_u128),
    ] {
        let Some(value) = object.get(name) else {
            continue;
        };
        let count = non_negative_int(value, name)?;
        let add = u128::from(count)
            .checked_mul(scale)
            .ok_or_else(|| "duration does not fit in milliseconds".to_string())?;
        total = total
            .checked_add(add)
            .ok_or_else(|| "duration does not fit in milliseconds".to_string())?;
    }
    for key in object.keys() {
        if !matches!(
            key.as_str(),
            "days" | "hours" | "minutes" | "seconds" | "milliseconds"
        ) {
            return Err(format!("unknown duration field `{key}`"));
        }
    }
    u64::try_from(total).map_err(|_| "duration does not fit in milliseconds".to_string())
}

fn non_negative_int(value: &Value, field: &str) -> Result<u64, String> {
    let Value::Number(number) = value else {
        return Err(format!("duration.{field} must be a non-negative integer"));
    };
    number
        .as_u64()
        .ok_or_else(|| format!("duration.{field} must be a non-negative integer"))
}

fn parse_iso_duration(text: &str) -> Result<u64, String> {
    let trimmed = text.trim();
    match parse_iso_parts(trimmed) {
        Ok(millis) => {
            u64::try_from(millis).map_err(|_| format!("`{trimmed}` does not fit in milliseconds"))
        }
        Err(reason) => Err(format!(
            "`{trimmed}` is not an ISO 8601 duration ({reason})"
        )),
    }
}

fn parse_iso_parts(text: &str) -> Result<u128, String> {
    let Some(rest) = text.strip_prefix('P') else {
        return Err("missing P".to_string());
    };
    if rest.is_empty() {
        return Err("missing components".to_string());
    }
    let mut total: u128 = 0;
    let mut saw = false;
    let rest = consume_units(
        rest,
        &[
            ('Y', MS_YEAR),
            ('M', MS_MONTH),
            ('W', MS_WEEK),
            ('D', MS_DAY),
        ],
        &mut total,
        &mut saw,
    )?;
    let rest = if let Some(time) = rest.strip_prefix('T') {
        if !time.starts_with(|ch: char| ch.is_ascii_digit()) {
            return Err("missing time components".to_string());
        }
        consume_units(
            time,
            &[('H', MS_HOUR), ('M', MS_MINUTE), ('S', MS_SECOND)],
            &mut total,
            &mut saw,
        )?
    } else {
        rest
    };
    if !rest.is_empty() {
        return Err(format!("unexpected `{rest}`"));
    }
    if !saw {
        return Err("missing components".to_string());
    }
    Ok(total)
}

fn consume_units<'a>(
    mut input: &'a str,
    mut allowed: &[(char, u128)],
    total: &mut u128,
    saw: &mut bool,
) -> Result<&'a str, String> {
    while input.starts_with(|ch: char| ch.is_ascii_digit()) {
        let (millis, rest, index) = parse_component(input, allowed)?;
        *total = total
            .checked_add(millis)
            .ok_or_else(|| "duration does not fit in milliseconds".to_string())?;
        *saw = true;
        allowed = &allowed[index + 1..];
        input = rest;
    }
    Ok(input)
}

fn parse_component<'a>(
    input: &'a str,
    allowed: &[(char, u128)],
) -> Result<(u128, &'a str, usize), String> {
    let (whole, frac, rest) = split_decimal(input)?;
    let mut chars = rest.chars();
    let Some(unit) = chars.next() else {
        return Err("missing a unit".to_string());
    };
    let Some(index) = allowed.iter().position(|(name, _)| *name == unit) else {
        return Err(format!("unexpected unit `{unit}`"));
    };
    let millis = decimal_millis(whole, frac, allowed[index].1)?;
    Ok((millis, &rest[unit.len_utf8()..], index))
}

fn split_decimal(input: &str) -> Result<(u128, &str, &str), String> {
    let digits_end = input
        .find(|ch: char| !ch.is_ascii_digit())
        .unwrap_or(input.len());
    if digits_end == 0 {
        return Err("missing a number".to_string());
    }
    let whole: u128 = input[..digits_end]
        .parse()
        .map_err(|_| "duration does not fit in milliseconds".to_string())?;
    let rest = &input[digits_end..];
    if let Some(after_dot) = rest.strip_prefix('.') {
        let frac_end = after_dot
            .find(|ch: char| !ch.is_ascii_digit())
            .unwrap_or(after_dot.len());
        if frac_end == 0 {
            return Err("empty fraction".to_string());
        }
        Ok((whole, &after_dot[..frac_end], &after_dot[frac_end..]))
    } else {
        Ok((whole, "", rest))
    }
}

fn decimal_millis(whole: u128, frac: &str, scale: u128) -> Result<u128, String> {
    let millis = whole
        .checked_mul(scale)
        .ok_or_else(|| "duration does not fit in milliseconds".to_string())?;
    if frac.is_empty() {
        return Ok(millis);
    }
    let bytes = frac.as_bytes();
    let mut numer: u128 = 0;
    let mut denom: u128 = 1;
    for digit in bytes.iter().take(9) {
        numer = numer * 10 + u128::from(digit - b'0');
        denom *= 10;
    }
    if bytes.len() > 9 && bytes[9] >= b'5' {
        numer += 1;
    }
    let product = numer
        .checked_mul(scale)
        .ok_or_else(|| "duration does not fit in milliseconds".to_string())?;
    let mut extra = product / denom;
    let remainder = product % denom;
    if remainder.saturating_mul(2) >= denom {
        extra += 1;
    }
    millis
        .checked_add(extra)
        .ok_or_else(|| "duration does not fit in milliseconds".to_string())
}

fn saturating_mul(left: u64, right: u64) -> u64 {
    saturating_u128(u128::from(left) * u128::from(right))
}

fn saturating_add(left: u64, right: u64) -> u64 {
    saturating_u128(u128::from(left) + u128::from(right))
}

fn saturating_u128(value: u128) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::BackoffKind;
    use super::JitterSample;
    use super::duration_millis;
    use super::retry_spec;
    use super::wait_ms;

    fn delay_of(policy: serde_json::Value, attempt: u64, sample: JitterSample) -> u64 {
        let spec = retry_spec(&policy).unwrap_or_else(|error| panic!("{error}"));
        let delay_ms = spec
            .delay
            .map(duration_millis)
            .transpose()
            .unwrap()
            .unwrap_or(0);
        let increment_ms = spec.increment.map(duration_millis).transpose().unwrap();
        let jitter = match (spec.jitter_from, spec.jitter_to) {
            (None, None) => None,
            (Some(from), Some(to)) => {
                Some((duration_millis(from).unwrap(), duration_millis(to).unwrap()))
            }
            _ => panic!("jitter bounds"),
        };
        wait_ms(delay_ms, spec.kind, increment_ms, jitter, attempt, sample)
            .unwrap_or_else(|error| panic!("{error}"))
    }

    #[test]
    fn iso_and_object_durations() {
        assert_eq!(duration_millis(&json!("PT3S")).unwrap(), 3_000);
        assert_eq!(duration_millis(&json!("PT1.5S")).unwrap(), 1_500);
        assert_eq!(duration_millis(&json!("PT1.2345S")).unwrap(), 1_235);
        assert_eq!(duration_millis(&json!("PT0.0005S")).unwrap(), 1);
        assert_eq!(duration_millis(&json!("PT1H2M3.5S")).unwrap(), 3_723_500);
        assert_eq!(duration_millis(&json!("P1DT2H")).unwrap(), 93_600_000);
        assert_eq!(duration_millis(&json!("P1W")).unwrap(), 604_800_000);
        assert_eq!(duration_millis(&json!("P1M")).unwrap(), 30 * 86_400_000);
        assert_eq!(duration_millis(&json!("PT1M")).unwrap(), 60_000);
        assert_eq!(duration_millis(&json!("P1Y")).unwrap(), 365 * 86_400_000);
        assert_eq!(duration_millis(&json!("  PT0S  ")).unwrap(), 0);
        assert_eq!(
            duration_millis(&json!({"minutes": 1, "seconds": 1, "milliseconds": 5})).unwrap(),
            61_005
        );
        assert!(duration_millis(&json!("PT")).is_err());
        assert!(duration_millis(&json!("P")).is_err());
        assert!(duration_millis(&json!("PT3S2S")).is_err());
        assert!(duration_millis(&json!("pT3S")).is_err());
        assert!(duration_millis(&json!({})).is_err());
        assert!(duration_millis(&json!({"seconds": -1})).is_err());
        assert!(duration_millis(&json!({"weeks": 1})).is_err());
    }

    #[test]
    fn backoff_series_and_jitter_sample() {
        let constant = json!({"delay": "PT2S", "backoff": {"constant": {}}});
        assert_eq!(delay_of(constant.clone(), 0, JitterSample::FROM), 2_000);
        assert_eq!(delay_of(constant, 4, JitterSample::FROM), 2_000);

        let linear = json!({"delay": {"seconds": 2}, "backoff": {"linear": {}}});
        assert_eq!(delay_of(linear.clone(), 0, JitterSample::FROM), 2_000);
        assert_eq!(delay_of(linear.clone(), 1, JitterSample::FROM), 4_000);
        assert_eq!(delay_of(linear, 2, JitterSample::FROM), 6_000);

        let stepped = json!({
            "delay": {"seconds": 1},
            "backoff": {"linear": {"increment": {"milliseconds": 250}}}
        });
        assert_eq!(delay_of(stepped.clone(), 0, JitterSample::FROM), 1_000);
        assert_eq!(delay_of(stepped, 2, JitterSample::FROM), 1_500);

        let exponential = json!({"delay": {"seconds": 1}, "backoff": {"exponential": {}}});
        assert_eq!(delay_of(exponential.clone(), 0, JitterSample::FROM), 1_000);
        assert_eq!(delay_of(exponential.clone(), 1, JitterSample::FROM), 2_000);
        assert_eq!(delay_of(exponential.clone(), 2, JitterSample::FROM), 4_000);
        assert_eq!(
            delay_of(exponential, 40, JitterSample::FROM),
            saturating_shift(1_000)
        );

        let omitted = json!({"delay": {"milliseconds": 25}});
        assert_eq!(delay_of(omitted, 3, JitterSample::FROM), 25);

        let jitter = json!({
            "delay": {"seconds": 1},
            "backoff": {"exponential": {}},
            "jitter": {"from": {"milliseconds": 100}, "to": {"milliseconds": 500}}
        });
        assert_eq!(delay_of(jitter.clone(), 0, JitterSample::FROM), 1_100);
        assert_eq!(delay_of(jitter.clone(), 0, JitterSample::TO), 1_500);
        let mid = JitterSample::new(500_000).unwrap();
        assert_eq!(delay_of(jitter.clone(), 0, mid), 1_300);
        assert_eq!(delay_of(jitter, 1, JitterSample::FROM), 2_100);
        assert!(JitterSample::new(1_000_001).is_none());
    }

    #[test]
    fn policy_shape_errors() {
        assert!(retry_spec(&json!({"backoff": {"constant": {}, "linear": {}}})).is_err());
        assert!(retry_spec(&json!({"backoff": {"exponential": {"factor": 2}}})).is_err());
        assert!(retry_spec(&json!({"jitter": {"from": "PT1S"}})).is_err());
        assert!(matches!(
            retry_spec(&json!({"delay": "PT1S"})).unwrap().kind,
            BackoffKind::Constant
        ));
        assert!(
            wait_ms(
                0,
                BackoffKind::Constant,
                None,
                Some((5, 1)),
                0,
                JitterSample::FROM
            )
            .is_err()
        );
    }

    fn saturating_shift(delay: u64) -> u64 {
        super::saturating_mul(delay, 1_u64 << 32)
    }
}
