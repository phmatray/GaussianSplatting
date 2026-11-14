//! Numbers as a French reader expects them: decimal comma, thin spaces
//! between thousands.

/// Narrow no-break space, the French thousands separator.
const THIN_SPACE: char = '\u{202f}';

/// `1234567` as `1 234 567`.
pub fn count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(THIN_SPACE);
        }
        out.push(c);
    }
    out
}

/// A number with `decimals` decimals and a decimal comma; a dash when it is
/// not finite.
pub fn decimal(value: f32, decimals: usize) -> String {
    if !value.is_finite() {
        return "-".to_string();
    }
    format!("{value:.decimals$}").replace('.', ",")
}

/// Milliseconds: one decimal below 10 ms, none above.
pub fn ms(value: f32) -> String {
    let decimals = if value.abs() < 10.0 { 1 } else { 0 };
    format!("{} ms", decimal(value, decimals))
}

/// A rate in hertz.
pub fn hz(value: f32) -> String {
    let decimals = if value.abs() < 10.0 { 1 } else { 0 };
    format!("{} Hz", decimal(value, decimals))
}

/// Frames per second.
pub fn fps(value: f32) -> String {
    let decimals = if value.abs() < 10.0 { 1 } else { 0 };
    format!("{} i/s", decimal(value, decimals))
}

/// A data rate given in bytes per second, as `ko/s` or `Mo/s`.
pub fn rate(bytes_per_second: f32) -> String {
    let kilo = bytes_per_second / 1000.0;
    if kilo >= 1000.0 {
        format!("{} Mo/s", decimal(kilo / 1000.0, 1))
    } else {
        format!("{} ko/s", decimal(kilo, if kilo < 10.0 { 1 } else { 0 }))
    }
}

/// A colon-separated fingerprint cut into groups of `per_group` bytes for
/// display, each group ending with its colon. The groups joined together
/// are the fingerprint again, so that a copy of the text stays exact.
pub fn fingerprint_groups(fingerprint: &str, per_group: usize) -> Vec<String> {
    let bytes: Vec<&str> = fingerprint.split(':').collect();
    let groups = bytes.chunks(per_group.max(1));
    let last = groups.len().saturating_sub(1);
    groups.enumerate().map(|(i, group)| if i == last { group.join(":") } else { format!("{}:", group.join(":")) }).collect()
}

/// A fraction in `0..=1` as a whole percentage.
pub fn percent(fraction: f32) -> String {
    let clamped = if fraction.is_finite() { fraction.clamp(0.0, 1.0) } else { 0.0 };
    // 99.6 % must not read as 100 % before the work is done.
    let whole = if clamped >= 1.0 { 100 } else { (clamped * 100.0).floor() as u32 };
    format!("{whole} %")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_are_grouped_by_three() {
        assert_eq!(count(0), "0");
        assert_eq!(count(999), "999");
        assert_eq!(count(1000), "1\u{202f}000");
        assert_eq!(count(1234567), "1\u{202f}234\u{202f}567");
    }

    #[test]
    fn decimal_comma() {
        assert_eq!(ms(3.12), "3,1 ms");
        assert_eq!(ms(42.7), "43 ms");
        assert_eq!(hz(19.96), "20 Hz");
        assert_eq!(fps(7.26), "7,3 i/s");
        assert_eq!(decimal(f32::NAN, 1), "-");
    }

    #[test]
    fn rates() {
        assert_eq!(rate(0.0), "0,0 ko/s");
        assert_eq!(rate(4_200.0), "4,2 ko/s");
        assert_eq!(rate(250_000.0), "250 ko/s");
        assert_eq!(rate(3_400_000.0), "3,4 Mo/s");
    }

    #[test]
    fn percent_never_rounds_up_to_done() {
        assert_eq!(percent(0.0), "0 %");
        assert_eq!(percent(0.996), "99 %");
        assert_eq!(percent(1.0), "100 %");
        assert_eq!(percent(7.0), "100 %");
        assert_eq!(percent(f32::NAN), "0 %");
    }

    #[test]
    fn fingerprints_break_between_bytes() {
        let fingerprint = (0..32).map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":");
        let groups = fingerprint_groups(&fingerprint, 8);
        assert_eq!(groups.len(), 4);
        assert_eq!(groups[0], "00:01:02:03:04:05:06:07:");
        assert_eq!(groups[3], "18:19:1A:1B:1C:1D:1E:1F");
        assert_eq!(groups.concat(), fingerprint);
        // Odd inputs are shown as they are.
        assert_eq!(fingerprint_groups("", 8), [""]);
        assert_eq!(fingerprint_groups("AB:CD", 0).concat(), "AB:CD");
        assert_eq!(fingerprint_groups("no colons here", 8), ["no colons here"]);
    }

    #[test]
    fn no_em_dash_anywhere() {
        for text in [ms(1.0), hz(1.0), rate(1.0), percent(0.5), decimal(f32::INFINITY, 0)] {
            assert!(!text.contains('\u{2014}'));
        }
    }
}
