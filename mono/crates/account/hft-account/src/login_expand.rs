//! `sigma001~sigma010` → `["sigma001", ..., "sigma010"]` 확장.
//!
//! csv 와 range 혼합도 지원한다.

use anyhow::{anyhow, Result};

/// 쉼표 구분 + `~` range 확장을 수행한다.
pub fn expand_logins(input: &str) -> Result<Vec<String>> {
    let mut result = Vec::new();
    for segment in input.split(',') {
        let segment = segment.trim();
        if segment.is_empty() {
            continue;
        }
        if let Some((left, right)) = segment.split_once('~') {
            result.extend(expand_range(left.trim(), right.trim())?);
        } else {
            result.push(segment.to_string());
        }
    }
    Ok(result)
}

fn expand_range(start: &str, end: &str) -> Result<Vec<String>> {
    let (prefix_s, num_s, width_s) = split_prefix_number(start)
        .ok_or_else(|| anyhow!("range start '{start}' has no numeric suffix"))?;
    let (prefix_e, num_e, width_e) = split_prefix_number(end)
        .ok_or_else(|| anyhow!("range end '{end}' has no numeric suffix"))?;
    if prefix_s != prefix_e {
        return Err(anyhow!(
            "range prefix mismatch: '{prefix_s}' vs '{prefix_e}'"
        ));
    }
    if num_s > num_e {
        return Err(anyhow!("range start {num_s} > end {num_e}"));
    }
    let width = width_s.max(width_e);
    let mut out = Vec::with_capacity((num_e - num_s + 1) as usize);
    for i in num_s..=num_e {
        out.push(format!("{prefix_s}{i:0width$}"));
    }
    Ok(out)
}

fn split_prefix_number(s: &str) -> Option<(&str, u64, usize)> {
    let digit_start = s.rfind(|c: char| !c.is_ascii_digit())? + 1;
    if digit_start >= s.len() {
        return None;
    }
    let prefix = &s[..digit_start];
    let numeric = &s[digit_start..];
    let width = numeric.len();
    let num = numeric.parse().ok()?;
    Some((prefix, num, width))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_range() {
        let result = expand_logins("sigma01~sigma03").expect("expand");
        assert_eq!(result, vec!["sigma01", "sigma02", "sigma03"]);
    }

    #[test]
    fn mixed_csv_and_range() {
        let result = expand_logins("alpha,sigma001~sigma003,beta").expect("expand");
        assert_eq!(
            result,
            vec!["alpha", "sigma001", "sigma002", "sigma003", "beta"]
        );
    }

    #[test]
    fn single_name() {
        let result = expand_logins("hello").expect("expand");
        assert_eq!(result, vec!["hello"]);
    }

    #[test]
    fn zero_padded_width_preserved() {
        let result = expand_logins("acc008~acc012").expect("expand");
        assert_eq!(
            result,
            vec!["acc008", "acc009", "acc010", "acc011", "acc012"]
        );
    }

    #[test]
    fn prefix_mismatch_error() {
        assert!(expand_logins("alpha01~beta03").is_err());
    }

    #[test]
    fn start_gt_end_error() {
        assert!(expand_logins("sigma10~sigma05").is_err());
    }

    #[test]
    fn empty_input() {
        let result = expand_logins("").expect("expand");
        assert!(result.is_empty());
    }
}
