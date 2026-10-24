//! `str.format(**kwargs)` for the `{name}` placeholders of UI message templates
//! (`{{` / `}}` are literal braces). Unknown placeholders are left as written.

pub fn format_named(text: &str, args: &[(&str, &str)]) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '{' && chars.get(i + 1) == Some(&'{') {
            out.push('{');
            i += 2;
        } else if c == '}' && chars.get(i + 1) == Some(&'}') {
            out.push('}');
            i += 2;
        } else if c == '{' {
            if let Some(end) = chars[i + 1..].iter().position(|&x| x == '}') {
                let name: String = chars[i + 1..i + 1 + end].iter().collect();
                match args.iter().find(|(k, _)| *k == name) {
                    Some((_, v)) => out.push_str(v),
                    None => {
                        out.push('{');
                        out.push_str(&name);
                        out.push('}');
                    }
                }
                i += end + 2;
            } else {
                out.push(c);
                i += 1;
            }
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

/// Python 3.12's `sum()` of floats: Neumaier compensated summation, so averages
/// match the Python dashboard to the last digit (a naive fold can differ).
pub fn py_sum(values: impl IntoIterator<Item = f64>) -> f64 {
    let (mut total, mut comp) = (0.0_f64, 0.0_f64);
    for x in values {
        let t = total + x;
        if total.abs() >= x.abs() {
            comp += (total - t) + x;
        } else {
            comp += (x - t) + total;
        }
        total = t;
    }
    if comp.is_finite() {
        total + comp
    } else {
        total
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitutes_and_unescapes() {
        assert_eq!(format_named("a {x} {{y}}", &[("x", "1")]), "a 1 {y}");
        assert_eq!(format_named("{nope}", &[]), "{nope}");
    }
}
