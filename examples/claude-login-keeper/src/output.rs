//! Reading `claude auth login` output: escape sequences stripped, the
//! authorize URL and the CLI's own verdict lines picked out.

/// Remove terminal escape sequences: CSI (`ESC [ ... final`), OSC
/// (`ESC ] ... BEL` or `ESC ] ... ESC \`, which carries OSC 8 hyperlinks)
/// and two-byte escapes. Other control bytes but `\n`, `\r`, `\t` become a
/// space. An escape cut off at the end of `bytes` is dropped; the caller
/// strips the whole retained buffer each time, so a sequence split across
/// reads is removed once its tail arrives.
pub fn strip_ansi(bytes: &[u8]) -> String {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == 0x1b {
            i += 1;
            match bytes.get(i) {
                Some(b'[') => {
                    i += 1;
                    while i < bytes.len() {
                        let c = bytes[i];
                        i += 1;
                        if (0x40..=0x7e).contains(&c) {
                            break;
                        }
                    }
                }
                Some(b']') => {
                    i += 1;
                    while i < bytes.len() {
                        if bytes[i] == 0x07 {
                            i += 1;
                            break;
                        }
                        if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'\\') {
                            i += 2;
                            break;
                        }
                        i += 1;
                    }
                }
                Some(_) => i += 1,
                None => {}
            }
            continue;
        }
        if b == b'\n' || b == b'\r' || b == b'\t' || b >= 0x20 && b != 0x7f {
            out.push(b);
        } else {
            out.push(b' ');
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The path every Claude authorize URL has, on `claude.com` and on the
/// older `claude.ai` / `console.anthropic.com` hosts alike.
const AUTHORIZE_PATH: &str = "/oauth/authorize?";

/// The last complete authorize URL in `text`. A URL is complete once
/// something follows it (whitespace), so a URL still arriving is not
/// returned truncated.
pub fn authorize_url(text: &str) -> Option<String> {
    let mut found = None;
    let mut rest = text;
    while let Some(start) = rest.find("https://") {
        let candidate = &rest[start..];
        let end = candidate.find(|c: char| c.is_whitespace() || c == '"' || c == '\'');
        let Some(end) = end else { break };
        let url = &candidate[..end];
        if url.contains(AUTHORIZE_PATH) {
            found = Some(url.to_owned());
        }
        rest = &candidate[end..];
    }
    found
}

/// The URL with every query value replaced by `…`, for logs that must not
/// carry the `state` or the PKCE challenge.
pub fn redact_query(url: &str) -> String {
    let Some((base, query)) = url.split_once('?') else {
        return url.to_owned();
    };
    let keys: Vec<String> = query
        .split('&')
        .map(|pair| match pair.split_once('=') {
            Some((k, _)) => format!("{k}=…"),
            None => pair.to_owned(),
        })
        .collect();
    format!("{base}?{}", keys.join("&"))
}

/// Lines in which the CLI reports a failed or refused login. The last one
/// seen becomes the flow's failure reason, and is shown while the flow
/// keeps waiting (an invalid pasted code is re-prompted, not fatal).
const NOTICES: &[&str] = &[
    "Login failed",
    "OAuth login failed",
    "Authentication failed",
    "Authorization failed",
    "Failed to sign in",
    "Invalid code",
    "That code is not from this sign-in",
    "could not be saved",
];

/// The last notice line in `text`, from its first notice phrase on (a
/// prompt without a newline may precede it), trimmed and bounded.
pub fn last_notice(text: &str) -> Option<String> {
    text.lines().rev().find_map(|line| {
        let start = NOTICES.iter().filter_map(|n| line.find(n)).min()?;
        Some(line[start..].trim().chars().take(240).collect())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csi_osc_and_hyperlinks_are_removed() {
        assert_eq!(strip_ansi(b"\x1b[31mhello\x1b[0m world\n"), "hello world\n");
        assert_eq!(strip_ansi(b"\x1b]0;title\x07after"), "after");
        // OSC 8: the hyperlink target disappears, the visible text stays.
        let link = b"visit: \x1b]8;;https://x.test/a\x1b\\https://x.test/a\x1b]8;;\x1b\\\n";
        assert_eq!(strip_ansi(link), "visit: https://x.test/a\n");
        assert_eq!(strip_ansi(b"a\x00b\x7fc"), "a b c");
    }

    #[test]
    fn utf8_survives_and_a_cut_escape_is_dropped() {
        assert_eq!(
            strip_ansi("Opening browser to sign in…".as_bytes()),
            "Opening browser to sign in…"
        );
        assert_eq!(strip_ansi(b"done\x1b[3"), "done");
        assert_eq!(strip_ansi(b"done\x1b"), "done");
    }

    #[test]
    fn the_authorize_url_is_found_only_once_complete() {
        let printed = "Opening browser to sign in…\nIf the browser didn't open, visit: \
            https://claude.com/cai/oauth/authorize?code=true&client_id=c&state=s\n";
        assert_eq!(
            authorize_url(printed).as_deref(),
            Some("https://claude.com/cai/oauth/authorize?code=true&client_id=c&state=s")
        );
        assert_eq!(
            authorize_url("visit: https://claude.com/cai/oauth/authorize?code=tr"),
            None
        );
        assert_eq!(
            authorize_url("see https://docs.example/help for more"),
            None
        );
    }

    #[test]
    fn query_values_are_redacted() {
        assert_eq!(
            redact_query("https://claude.com/cai/oauth/authorize?code=true&state=abc&flag"),
            "https://claude.com/cai/oauth/authorize?code=…&state=…&flag"
        );
        assert_eq!(redact_query("https://claude.com/x"), "https://claude.com/x");
    }

    #[test]
    fn the_last_notice_wins() {
        let text = "Paste code here if prompted > Invalid code. Please make sure the full code was copied.\n";
        assert_eq!(
            last_notice(text).as_deref(),
            Some("Invalid code. Please make sure the full code was copied.")
        );
        let text =
            format!("{text}Paste code here if prompted > \nLogin failed: token exchange refused\n");
        assert_eq!(
            last_notice(&text).as_deref(),
            Some("Login failed: token exchange refused")
        );
        assert_eq!(last_notice("Opening browser to sign in…"), None);
    }
}
