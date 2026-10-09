//! The system's LC_TIME locale: the names of months and days, in the
//! clock's dates and in the calendar's grid, follow it, as any
//! localised program's do.

use chrono::Locale;

/// LC_ALL, else LC_TIME, else LANG (an "it_IT.UTF-8" is its "it_IT",
/// then a bare "it" if that is a known locale); POSIX when none is.
pub fn time() -> Locale {
    for var in ["LC_ALL", "LC_TIME", "LANG"] {
        let Ok(value) = std::env::var(var) else {
            continue;
        };
        let name = value.split(['.', '@']).next().unwrap_or_default();
        if let Ok(locale) = Locale::try_from(name) {
            return locale;
        }
        if let Some((lang, _)) = name.split_once('_') {
            if let Ok(locale) = Locale::try_from(lang) {
                return locale;
            }
        }
    }
    Locale::POSIX
}

#[cfg(test)]
mod tests {
    #[test]
    fn follows_lc_all() {
        // LC_ALL wins, the encoding suffix is dropped, names are localised
        std::env::set_var("LC_ALL", "it_IT.UTF-8");
        let locale = super::time();
        assert_eq!(locale, chrono::Locale::it_IT);
        let august = chrono::NaiveDate::from_ymd_opt(2026, 8, 1).unwrap();
        assert_eq!(august.format_localized("%B", locale).to_string(), "agosto");
        assert_eq!(august.format_localized("%a", locale).to_string(), "sab");
        std::env::remove_var("LC_ALL");
    }
}
