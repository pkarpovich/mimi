pub const VERSION_MARKER: &str = "__VERSION__";

/// render substitutes the version marker in an Info.plist template.
pub fn render(template: &str, version: &str) -> String {
    template.replace(VERSION_MARKER, version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitutes_every_occurrence() {
        let template = "<string>__VERSION__</string><string>__VERSION__</string>";
        assert_eq!(
            render(template, "0.1.0"),
            "<string>0.1.0</string><string>0.1.0</string>"
        );
    }

    #[test]
    fn leaves_other_content_untouched() {
        let template = "<key>CFBundleIdentifier</key><string>dev.pkarpovich.mimi</string>";
        assert_eq!(render(template, "9.9.9"), template);
    }

    #[test]
    fn template_without_marker_is_unchanged() {
        let template = "no marker here";
        assert_eq!(render(template, "0.1.0"), template);
    }

    #[test]
    fn empty_version_removes_the_marker() {
        assert_eq!(
            render("<string>__VERSION__</string>", ""),
            "<string></string>"
        );
    }
}
