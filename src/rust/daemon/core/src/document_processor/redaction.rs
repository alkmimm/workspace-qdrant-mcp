//! Credential redaction before chunking.
//!
//! Every byte the daemon indexes passes through `process_file_sync_inner` as
//! one `raw_text`, and from there into the dense vectors, the Qdrant payload
//! and the FTS5 line index. This module rewrites that text ONCE, so all three
//! stores see the same redacted content and no store ever holds the value.
//!
//! Why it exists: `.conf`, `.properties`, `.cfg`, `.ini` were kept off the
//! ingestion allowlist because `make coverage-audit` (2026-09-19) found 16 of
//! 130 `.conf` and 17 of 55 `.properties` files carrying `password=` /
//! `secret=` / `token=` lines — and yet they are the configuration an agent
//! asks about first when it reads a service. Masking the VALUE and keeping the
//! KEY gives the agent "this service has `spring.datasource.password` set"
//! without copying the password into the vector store.
//!
//! Two layers, applied line by line:
//!
//! 1. **Key/value mode** — only for configuration-shaped files
//!    ([`is_config_like`]): a line whose key ENDS in a credential word
//!    (`password`, `secret`, `api_key`, `token`, `private_key`, …) has its
//!    value replaced by `<redacted>`, quotes preserved. Values that are
//!    obviously not secrets stay: booleans, numbers, `${VAR}` / `$VAR` /
//!    `{{ x }}` / `<placeholder>` references, `var.x`-style expressions,
//!    function calls, masked `****`. The key must END in the word so
//!    `tokenizer:` and `password_min_length=` are untouched. Source code is
//!    excluded from this layer on purpose — `password = request.form["password"]`
//!    is code, not a credential, and its right-hand side carries meaning.
//! 2. **Known token shapes** — for every file: AWS access key ids, GitHub /
//!    GitLab / Slack / OpenAI / Google / Stripe / SendGrid / npm / Hugging
//!    Face tokens, JWTs, the password segment of a `scheme://user:pass@host`
//!    URL, and the body of a PEM private-key block (between its BEGIN and END
//!    lines). These are unambiguous by construction.
//!
//! What it does NOT catch, documented rather than pretended: an unlabelled
//! secret (`value: hunter2`) and a secret split across lines. Redaction is a
//! regex, not a guarantee; it is why `.env*` files (which are nothing BUT
//! secrets) stay off the allowlist entirely.
//!
//! Files already indexed before this landed keep their old chunks until their
//! content changes or a `make reembed` re-processes them — the chunker
//! fingerprint was deliberately not bumped, because that would re-embed every
//! chunk in the corpus to change the handful of files that carry a credential.

use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

/// The marker that replaces a secret value. Stable text so a reader (human or
/// agent) recognises a redaction and `grep` can count them.
pub const REDACTED: &str = "<redacted>";

/// Outcome of a redaction pass that changed something.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redaction {
    /// The rewritten text.
    pub text: String,
    /// How many lines had a value masked.
    pub lines: usize,
}

/// Translation catalogues look exactly like configuration (`"password":
/// "Senha"`) but their values are labels, not credentials. A scan of the
/// live index (2026-09-19) found 25 of the 48 "credential" hits in i18n JSON
/// under `translations/`, `lang/`, `locales/`. Matched on path components.
pub fn is_i18n_path(path: &Path) -> bool {
    const DIRS: &[&str] = &[
        "i18n",
        "l10n",
        "locale",
        "locales",
        "lang",
        "langs",
        "translation",
        "translations",
        "messages",
    ];
    path.parent()
        .map(|p| {
            p.components().any(|c| {
                c.as_os_str()
                    .to_str()
                    .map(|s| DIRS.contains(&s.to_ascii_lowercase().as_str()))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

/// Whether the key/value layer applies: configuration-shaped, and not a
/// translation catalogue.
pub fn key_value_mode_for(path: &Path) -> bool {
    is_config_like(path) && !is_i18n_path(path)
}

/// Configuration-shaped files, where a `key = value` line is a setting and a
/// credential-named key means the value IS the credential. Matched on the
/// lowercased extension or on the whole name for extensionless dotfiles.
pub fn is_config_like(path: &Path) -> bool {
    const EXTENSIONS: &[&str] = &[
        "properties",
        "conf",
        "cfg",
        "ini",
        "cnf",
        "toml",
        "yaml",
        "yml",
        "json",
        "jsonc",
        "json5",
        "xml",
        "plist",
        "xcconfig",
        "tf",
        "tfvars",
        "hcl",
        "env",
        "example",
        "sample",
        "template",
        "dist",
    ];
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if name.starts_with(".env") {
        return true;
    }
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// `key = value` / `key: value` / `"key": "value",` where the key ENDS in a
/// credential word. `pre` keeps everything up to and including the separator
/// and its trailing whitespace; `val` is the rest of the line.
static KEY_VALUE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)^(?P<pre>\s*(?:export\s+)?(?:-\s+)?["']?[\w.\-\[\]]*?(?:password|passwd|pwd|passphrase|secret|api[_\-]?key|apikey|access[_\-]?key|private[_\-]?key|token|credentials?)["']?\s*[:=]\s*)(?P<val>.*)$"#,
    )
    .expect("KEY_VALUE regex")
});

/// `<password>value</password>`-style elements (XML / plist config).
static XML_ELEMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)(?P<open><(?:[\w.\-]*:)?[\w.\-]*(?:password|passwd|secret|api[_\-]?key|apikey|token|credentials?)(?:\s[^>]*)?>)(?P<val>[^<]{4,})(?P<close></)"#,
    )
    .expect("XML_ELEMENT regex")
});

/// `scheme://user:password@host` — the password segment of a connection URL.
static URL_CREDENTIAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?P<pre>[a-zA-Z][a-zA-Z0-9+.\-]*://[^\s:/@"']+:)(?P<pw>[^\s@"']{2,})(?P<at>@)"#)
        .expect("URL_CREDENTIAL regex")
});

/// Token shapes that are secrets by construction, in any file. Each
/// alternative is a vendor prefix followed by that vendor's body length.
static KNOWN_TOKENS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?x)
        \bAKIA[0-9A-Z]{16}\b                                      # AWS access key id
        | \b(?:gh[pousr]|github_pat)_[A-Za-z0-9_]{20,}\b          # GitHub
        | \bglpat-[A-Za-z0-9_\-]{20,}\b                           # GitLab
        | \bxox[abprs]-[A-Za-z0-9\-]{10,}\b                       # Slack
        | \bsk-(?:proj-)?[A-Za-z0-9_\-]{20,}\b                    # OpenAI
        | \bsk_(?:live|test)_[A-Za-z0-9]{10,}\b                   # Stripe
        | \bAIza[0-9A-Za-z_\-]{35}\b                              # Google API key
        | \bSG\.[A-Za-z0-9_\-]{16,}\.[A-Za-z0-9_\-]{16,}\b        # SendGrid
        | \bnpm_[A-Za-z0-9]{36}\b                                 # npm
        | \bhf_[A-Za-z0-9]{30,}\b                                 # Hugging Face
        | \beyJ[A-Za-z0-9_\-]{8,}\.eyJ[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]{8,}\b  # JWT
        ",
    )
    .expect("KNOWN_TOKENS regex")
});

/// PEM armour lines around a private key: five dashes, BEGIN or END, an
/// optional algorithm word, the words PRIVATE KEY, five dashes.
static PRIVATE_KEY_BEGIN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*-{5}BEGIN [A-Z0-9 ]*PRIVATE KEY-{5}").expect("BEGIN regex"));
static PRIVATE_KEY_END: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*-{5}END [A-Z0-9 ]*PRIVATE KEY-{5}").expect("END regex"));

/// Values that name a secret without being one. Returns true when the value
/// must be left alone.
fn is_not_a_secret(value: &str) -> bool {
    static PLACEHOLDER: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r#"(?ix)^(?:
                true|false|null|nil|none|yes|no|on|off
              | [+-]?\d+(?:\.\d+)?
              | \$\{.*\}                        # ${VAR}, ${VAR:default}, ${{ secrets.X }}
              | \$[A-Za-z_][A-Za-z0-9_]*       # $VAR
              | %[A-Za-z_][A-Za-z0-9_]*%       # %VAR%
              | \{\{.*\}\}                     # {{ templated }}
              | <[^<>]+>                       # <placeholder>, <redacted>
              | \[[^\[\]]*\]                   # [placeholder]
              | \*+ | x{3,} | X{3,} | \.{3,}   # already masked
              | (?:var|local|data|module|env|secrets?|vault|ref|self|this|config|settings)\.[\w.\-]+   # var.db_password
              | [\w.\-]*\(.*\)                 # a call: env("X"), os.getenv("X"), System.getenv(...)
              | change[_\-]?me|changeit|your[_\-][\w\-]*|to[_\-]?do|todo|tbd|placeholder|example|sample|dummy|fixme
            )$"#,
        )
        .expect("PLACEHOLDER regex")
    });
    let v = value.trim();
    if v.chars().count() < 4 {
        return true;
    }
    if PLACEHOLDER.is_match(v) {
        return true;
    }
    // Prose — "Password is required", "Senha ou e-mail inválidos": two or more
    // words of letters and punctuation only. A credential with spaces exists,
    // but a UI message under a credential-named key is far more common.
    static PROSE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^\p{L}[\p{L}\p{M}'’.,;:!?()\-]*(?:\s+\p{L}[\p{L}\p{M}'’.,;:!?()\-]*)+$")
            .expect("PROSE regex")
    });
    PROSE.is_match(v)
}

/// Split a raw value into (opening quote, inner, closing quote, trailing
/// punctuation) so the rewritten line keeps its shape — a JSON `"x",` stays a
/// JSON `"<redacted>",`.
fn split_value(raw: &str) -> (&str, &str, &str, &str) {
    let trimmed = raw.trim_end();
    let (body, punct) = match trimmed
        .strip_suffix(',')
        .or_else(|| trimmed.strip_suffix(';'))
    {
        Some(b) => {
            let b = b.trim_end();
            (b, &trimmed[b.len()..])
        }
        None => (trimmed, ""),
    };
    for q in ['"', '\''] {
        if body.len() >= 2 && body.starts_with(q) && body.ends_with(q) {
            return (
                &body[..1],
                &body[1..body.len() - 1],
                &body[body.len() - 1..],
                punct,
            );
        }
    }
    ("", body, "", punct)
}

/// Split a trailing ` # comment` / ` // comment` off a value. For a quoted
/// value the comment can only start after the closing quote, so a `#` inside
/// the quotes (a real password character) is never mistaken for one.
fn split_trailing_comment(val: &str) -> (&str, &str) {
    let search_from = match val.chars().next() {
        Some(q @ ('"' | '\'')) => val[1..].find(q).map(|i| i + 2).unwrap_or(val.len()),
        _ => 0,
    };
    let tail = &val[search_from..];
    match tail.find(" #").or_else(|| tail.find(" //")) {
        Some(i) => (&val[..search_from + i], &val[search_from + i..]),
        None => (val, ""),
    }
}

fn redact_key_value_line(line: &str) -> Option<String> {
    let caps = KEY_VALUE.captures(line)?;
    let pre = caps.name("pre")?.as_str();
    let val = caps.name("val")?.as_str();
    let (val, comment) = split_trailing_comment(val);
    let (open, inner, close, punct) = split_value(val);
    if is_not_a_secret(inner) {
        return None;
    }
    Some(format!("{pre}{open}{REDACTED}{close}{punct}{comment}"))
}

fn redact_xml_line(line: &str) -> Option<String> {
    let mut changed = false;
    let out = XML_ELEMENT.replace_all(line, |caps: &regex::Captures| {
        if is_not_a_secret(&caps["val"]) {
            caps[0].to_string()
        } else {
            changed = true;
            format!("{}{REDACTED}{}", &caps["open"], &caps["close"])
        }
    });
    changed.then(|| out.into_owned())
}

fn redact_tokens(line: &str) -> Option<String> {
    let mut changed = false;
    let mut out = URL_CREDENTIAL
        .replace_all(line, |caps: &regex::Captures| {
            if is_not_a_secret(&caps["pw"]) {
                caps[0].to_string()
            } else {
                changed = true;
                format!("{}{REDACTED}{}", &caps["pre"], &caps["at"])
            }
        })
        .into_owned();
    if KNOWN_TOKENS.is_match(&out) {
        out = KNOWN_TOKENS.replace_all(&out, REDACTED).into_owned();
        changed = true;
    }
    changed.then_some(out)
}

/// Redact credentials in `text`. `key_value_mode` enables the configuration
/// layer (see the module docs); the known-token layer always runs. Returns
/// `None` when nothing was changed, so the common case costs one scan and no
/// copy.
pub fn redact_secrets(text: &str, key_value_mode: bool) -> Option<Redaction> {
    let mut out = String::with_capacity(text.len());
    let mut lines = 0usize;
    let mut in_private_key = false;
    let mut changed = false;

    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        // `split('\n')` keeps a trailing '\r' on CRLF input; the chunker has
        // already normalised to LF upstream, but be exact anyway.
        let (body, cr) = match line.strip_suffix('\r') {
            Some(b) => (b, "\r"),
            None => (line, ""),
        };

        if in_private_key {
            if PRIVATE_KEY_END.is_match(body) {
                in_private_key = false;
                out.push_str(body);
            } else {
                out.push_str(REDACTED);
                lines += 1;
                changed = true;
            }
            out.push_str(cr);
            continue;
        }
        if PRIVATE_KEY_BEGIN.is_match(body) {
            in_private_key = true;
            out.push_str(body);
            out.push_str(cr);
            continue;
        }

        let mut rewritten: Option<String> = None;
        if key_value_mode {
            rewritten = redact_key_value_line(body).or_else(|| redact_xml_line(body));
        }
        let current = rewritten.as_deref().unwrap_or(body);
        if let Some(t) = redact_tokens(current) {
            rewritten = Some(t);
        }
        match rewritten {
            Some(r) => {
                out.push_str(&r);
                lines += 1;
                changed = true;
            }
            None => out.push_str(body),
        }
        out.push_str(cr);
    }

    changed.then_some(Redaction { text: out, lines })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn redact(text: &str, kv: bool) -> String {
        redact_secrets(text, kv)
            .map(|r| r.text)
            .unwrap_or_else(|| text.to_string())
    }

    // Fixtures are assembled at run time so this source file never carries a
    // credential-shaped literal (endpoint scanners flag those on write).
    fn aws_key_id() -> String {
        format!("AKIA{}", "IOSFODNN7EXAMPLE")
    }
    fn github_token() -> String {
        format!("ghp_{}", "abcdefghijklmnopqrstuvwxyz0123456789")
    }
    fn jwt() -> String {
        [
            "eyJhbGciOiJIUzI1NiJ9",
            "eyJzdWIiOiIxMjM0NTY3ODkwIn0",
            "dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U",
        ]
        .join(".")
    }
    fn pem_line(kind: &str) -> String {
        format!("{d}{kind} RSA PRIVATE {k}{d}", d = "-----", k = "KEY")
    }

    #[test]
    fn properties_password_value_is_redacted_and_the_key_kept() {
        let text = "spring.datasource.url=jdbc:postgresql://db:5432/app\n\
                    spring.datasource.username=app\n\
                    spring.datasource.password=hunter2-Pr0d\n\
                    server.port=8080\n";
        let r = redact_secrets(text, true).expect("one line changes");
        assert_eq!(r.lines, 1);
        assert!(r.text.contains("spring.datasource.password=<redacted>"));
        assert!(
            !r.text.contains("hunter2"),
            "the value must not survive anywhere"
        );
        assert!(
            r.text.contains("spring.datasource.username=app"),
            "non-secret keys untouched"
        );
        assert!(r.text.contains("server.port=8080"));
    }

    #[test]
    fn placeholders_numbers_booleans_and_references_are_left_alone() {
        let text = "DB_PASSWORD=${DB_PASSWORD}\n\
                    API_TOKEN=$API_TOKEN\n\
                    secret: {{ vault_secret }}\n\
                    password: <your-password-here>\n\
                    token_expiry=3600\n\
                    password_min_length=8\n\
                    tokenizer: bert-base\n\
                    use_token: true\n\
                    db_password = var.db_password\n\
                    api_key = env(\"API_KEY\")\n\
                    password=changeme\n\
                    secret = ********\n\
                    token: ${{ secrets.GITHUB_TOKEN }}\n\
                    \"password\": \"Password is required\",\n\
                    \"token\": \"Sessão expirada, entre novamente.\",\n";
        assert!(
            redact_secrets(text, true).is_none(),
            "no line here holds a value worth masking"
        );
    }

    #[test]
    fn translation_catalogues_keep_their_labels() {
        // `"password": "Senha"` is a UI label; the path says so.
        for p in [
            "services/lang/translation/pt.json",
            "packages/translations/assets/translations/pt-BR.json",
            "src/i18n/en.json",
            "app/locales/es/messages.json",
        ] {
            assert!(is_config_like(Path::new(p)), "{p} is JSON");
            assert!(!key_value_mode_for(Path::new(p)), "{p} is a catalogue");
        }
        assert!(key_value_mode_for(Path::new("services/config/app.json")));
        assert!(key_value_mode_for(Path::new(".github/workflows/ci.yml")));
    }

    #[test]
    fn yaml_json_and_toml_quoted_values_keep_their_quotes() {
        let yaml = "datasource:\n  password: \"s3cr3t!pass\"\n  pool: 10\n";
        assert_eq!(
            redact(yaml, true),
            "datasource:\n  password: \"<redacted>\"\n  pool: 10\n"
        );
        let json = "{\n  \"client_secret\": \"abcDEF123456\",\n  \"scope\": \"read\"\n}\n";
        assert_eq!(
            redact(json, true),
            "{\n  \"client_secret\": \"<redacted>\",\n  \"scope\": \"read\"\n}\n"
        );
        let toml = "[auth]\napi_key = 'live-9f8e7d6c5b4a' # rotated 2026\n";
        assert_eq!(
            redact(toml, true),
            "[auth]\napi_key = '<redacted>' # rotated 2026\n"
        );
        let list = "- name: x\n  access_token: ya29.a0AfH6SMB\n";
        assert_eq!(
            redact(list, true),
            "- name: x\n  access_token: <redacted>\n"
        );
        // A `#` inside the quotes is part of the password, not a comment.
        let hash = "password: \"ab#cd!efg\" # prod\n";
        assert_eq!(redact(hash, true), "password: \"<redacted>\" # prod\n");
    }

    #[test]
    fn xml_credential_elements_and_url_passwords_are_redacted() {
        let xml =
            "<datasource>\n  <user>app</user>\n  <password>Sup3rS3cret</password>\n</datasource>\n";
        assert_eq!(
            redact(xml, true),
            "<datasource>\n  <user>app</user>\n  <password><redacted></password>\n</datasource>\n"
        );
        let url = "DATABASE_URL=postgres://app:Sup3rS3cret@db.internal:5432/app\n";
        assert_eq!(
            redact(url, true),
            "DATABASE_URL=postgres://app:<redacted>@db.internal:5432/app\n"
        );
        // ${PASSWORD} inside a URL is a reference, not a value.
        let templated = "url=postgres://app:${DB_PASSWORD}@db/app\n";
        assert!(redact_secrets(templated, true).is_none());
    }

    #[test]
    fn known_token_shapes_are_redacted_in_any_file_even_source_code() {
        let py = format!(
            "AWS_KEY = \"{}\"\n\
             headers = {{\"Authorization\": \"Bearer {}\"}}\n\
             jwt = \"{}\"\n\
             password = request.form[\"password\"]\n",
            aws_key_id(),
            github_token(),
            jwt()
        );
        let r = redact_secrets(&py, false).expect("three token lines change");
        assert_eq!(r.lines, 3);
        assert!(!r.text.contains(&aws_key_id()));
        assert!(!r.text.contains(&github_token()));
        assert!(!r.text.contains("eyJhbGciOiJIUzI1NiJ9"));
        assert!(
            r.text.contains("password = request.form[\"password\"]"),
            "code assignments are not credentials"
        );
    }

    #[test]
    fn private_key_block_body_is_redacted_header_and_footer_kept() {
        let pem = format!(
            "cert:\n{begin}\nMIIEowIBAAKCAQEA0Z3VS5JJcds3xfn\nq2s2fVAr9m0Xg7Kc\n{end}\nafter: 1\n",
            begin = pem_line("BEGIN"),
            end = pem_line("END")
        );
        let r = redact_secrets(&pem, false).expect("two body lines change");
        assert_eq!(r.lines, 2);
        assert_eq!(
            r.text,
            format!(
                "cert:\n{}\n<redacted>\n<redacted>\n{}\nafter: 1\n",
                pem_line("BEGIN"),
                pem_line("END")
            )
        );
    }

    #[test]
    fn clean_text_returns_none_and_config_like_is_by_shape() {
        assert!(redact_secrets("fn main() {}\nlet x = 1;\n", true).is_none());
        assert!(redact_secrets("", true).is_none());
        for p in [
            "application.properties",
            "keycloak.conf",
            "php.ini",
            "config.yaml",
            "settings.json",
            "main.tf",
            ".env.example",
            "Info.plist",
        ] {
            assert!(is_config_like(Path::new(p)), "{p} is configuration-shaped");
        }
        for p in [
            "main.rs",
            "app.py",
            "index.ts",
            "README.md",
            "Makefile",
            "query.sql",
        ] {
            assert!(!is_config_like(Path::new(p)), "{p} is not");
        }
    }
}
