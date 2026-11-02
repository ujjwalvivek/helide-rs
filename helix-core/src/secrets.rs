//! Secrets: which files hold them, where their values sit on a line, and how to keep them out
//! of text that leaves the editor.
//!
//! `.env`-style files are parsed line by line and only secret values count (`PORT=8080` stays
//! visible, `API_KEY=…` does not). Files that are a secret as a whole (`id_rsa`, `*.pem`,
//! `passwords.txt`) count on every line. Anywhere else, only well-known token shapes count (see
//! [`redact_tokens`]).

use std::{borrow::Cow, ops::Range, path::Path, sync::LazyLock};

use regex::Regex;

/// What replaces a secret in text sent out of the editor.
pub const REDACTED: &str = "[REDACTED]";

/// How a file holds secrets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretFile {
    /// `KEY=value` lines; secret values are found per line.
    Env,
    /// The whole file is a secret.
    Whole,
}

/// Judges a file by its name.
pub fn classify(path: &Path) -> Option<SecretFile> {
    let name = path.file_name()?.to_str()?.to_ascii_lowercase();
    if is_env_file(&name) {
        Some(SecretFile::Env)
    } else if is_secret_file(&name) {
        Some(SecretFile::Whole)
    } else {
        None
    }
}

/// `.env`, `.env.local`, `.env.production`, `prod.env` …, and the key=value files that hold
/// registry tokens (`.npmrc`, `.yarnrc`, `.pypirc`).
pub fn is_env_file(name_lower: &str) -> bool {
    matches!(name_lower, ".npmrc" | ".yarnrc" | ".pypirc")
        || name_lower == ".env"
        || name_lower.starts_with(".env.")
        || name_lower.ends_with(".env")
}

/// Files that are a secret as a whole, judged by name: private keys (`id_rsa`, `*.pem`,
/// `*.key`), `.netrc` and friends, and text files named for what they hold (`passwords.txt`,
/// `api-token`, `prod.secret`). Code files keep their language, so `auth.go` and `token.rs` are
/// not caught.
pub fn is_secret_file(name_lower: &str) -> bool {
    const NAMES: &[&str] = &[
        "id_rsa",
        "id_dsa",
        "id_ecdsa",
        "id_ed25519",
        ".netrc",
        "_netrc",
        ".pgpass",
        ".htpasswd",
        ".git-credentials",
        "credentials",
    ];
    const EXTENSIONS: &[&str] = &[
        "pem",
        "key",
        "ppk",
        "p8",
        "secret",
        "secrets",
        "token",
        "password",
        "passwords",
    ];
    if NAMES.contains(&name_lower) {
        return true;
    }
    let dot = name_lower.rfind('.');
    let ext = dot.map_or("", |d| &name_lower[d + 1..]);
    if EXTENSIONS.contains(&ext) {
        return true;
    }
    if dot.is_some_and(|d| d > 0) && ext != "txt" && ext != "text" {
        return false;
    }
    let stem = match dot {
        Some(d) if d > 0 => &name_lower[..d],
        _ => name_lower,
    };
    GLUED.iter().any(|word| stem.contains(word))
        || stem
            .split(['_', '-', '.', ' '])
            .any(|word| FILE_WORDS.contains(&word))
}

// For file names only: words that are too broad for an .env key (ENCRYPTION_ENABLED) but mean
// the contents in "encryption.txt".
const FILE_WORDS: &[&str] = &[
    "secrets",
    "tokens",
    "passwords",
    "pwd",
    "creds",
    "cred",
    "key",
    "keys",
    "apikeys",
    "private",
    "encryption",
    "recovery",
    "seed",
    "mnemonic",
    "otp",
    "totp",
    "mfa",
    "2fa",
    "pin",
    "pins",
    "auth",
    "api",
];

// Words that name a secret on their own. Broader ones (SESSION, ACCESS, CONNECTION, CERT) also
// name timeouts, flags and paths, so they only count next to one of these (SESSION_SECRET,
// ACCESS_KEY).
const SECRET_WORDS: &[&str] = &[
    "api",
    "apikey",
    "key",
    "keys",
    "secret",
    "secrets",
    "token",
    "tokens",
    "pass",
    "password",
    "passwd",
    "pwd",
    "passphrase",
    "auth",
    "oauth",
    "bearer",
    "jwt",
    "credential",
    "credentials",
    "cred",
    "creds",
    "private",
    "privatekey",
    "salt",
    "hmac",
    "signature",
    "webhook",
    "dsn",
    "connstr",
    "accesskey",
    "clientsecret",
    "secretkey",
    "apisecret",
    "masterkey",
    "otp",
    "totp",
];

/// Glued forms: a key word inside one camel-free token ("APIKEY", "DBPASSWORD").
const GLUED: &[&str] = &[
    "secret",
    "password",
    "passwd",
    "token",
    "apikey",
    "privatekey",
    "credential",
    "passphrase",
];

/// The secret part of one line of a [`SecretFile::Env`] file (without its line end), as a byte
/// range: the value of a secret key, a value that looks like a credential, or a bare token on a
/// line of its own.
pub fn env_line_secret(line: &str) -> Option<Range<usize>> {
    let bytes = line.as_bytes();
    let blank = |b: u8| b == b' ' || b == b'\t';
    let mut i = 0;
    while i < bytes.len() && blank(bytes[i]) {
        i += 1;
    }
    if i >= bytes.len() || bytes[i] == b'#' || bytes[i] == b';' {
        return None;
    }
    if line[i..].starts_with("export ") {
        i += 7;
        while i < bytes.len() && bytes[i] == b' ' {
            i += 1;
        }
    }
    let key_start = i;
    i = key_end(bytes, key_start);
    let mut key_end = i;
    while key_end > key_start && blank(bytes[key_end - 1]) {
        key_end -= 1;
    }
    let key = &line[key_start..key_end];
    if i >= bytes.len() {
        // No "=": a bare token pasted on its own line is still a secret.
        return (!key.is_empty() && looks_secret(key)).then_some(key_start..key_end);
    }
    i += 1;
    while i < bytes.len() && blank(bytes[i]) {
        i += 1;
    }
    let mut value_end = bytes.len();
    // An unquoted value ends at " #comment".
    if i < bytes.len() && bytes[i] != b'"' && bytes[i] != b'\'' {
        if let Some(k) = (i + 1..bytes.len()).find(|&k| bytes[k] == b'#' && blank(bytes[k - 1])) {
            value_end = k;
        }
    }
    while value_end > i && blank(bytes[value_end - 1]) {
        value_end -= 1;
    }
    if value_end <= i {
        return None;
    }
    let value = &line[i..value_end];
    (is_secret_key(key) || looks_secret(unquote(value))).then_some(i..value_end)
}

/// One line of a [`SecretFile::Whole`] file: everything but the indent and trailing blanks.
pub fn whole_line_secret(line: &str) -> Option<Range<usize>> {
    let trimmed = line.trim_matches([' ', '\t']);
    if trimmed.is_empty() {
        return None;
    }
    let start = line.len() - line.trim_start_matches([' ', '\t']).len();
    Some(start..start + trimmed.len())
}

/// The secret part of one line of a file of the given kind.
pub fn line_secret(kind: SecretFile, line: &str) -> Option<Range<usize>> {
    match kind {
        SecretFile::Env => env_line_secret(line),
        SecretFile::Whole => whole_line_secret(line),
    }
}

/// Where the key ends: at the first '=', or at a ':' that comes before it and is followed by a
/// space or the line end (`KEY: value`). Other colons belong to the key, as in .npmrc's
/// `//registry.npmjs.org/:_authToken=`.
fn key_end(line: &[u8], from: usize) -> usize {
    (from..line.len())
        .find(|&i| {
            line[i] == b'='
                || (line[i] == b':'
                    && (i + 1 == line.len() || line[i + 1] == b' ' || line[i + 1] == b'\t'))
        })
        .unwrap_or(line.len())
}

fn unquote(value: &str) -> &str {
    let bytes = value.as_bytes();
    if bytes.len() >= 2
        && (bytes[0] == b'"' || bytes[0] == b'\'')
        && bytes[bytes.len() - 1] == bytes[0]
    {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

/// Splits the key into words at `_ - .` and camelCase; any secret word matches. `DATABASE_URL`
/// and friends count too (they carry passwords).
pub fn is_secret_key(key: &str) -> bool {
    if key.is_empty() || key.len() > 128 {
        return false;
    }
    let lower = key.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "database_url"
            | "db_url"
            | "redis_url"
            | "mongo_url"
            | "mongodb_uri"
            | "amqp_url"
            | "connection_string"
    ) {
        return true;
    }
    if GLUED.iter().any(|word| lower.contains(word)) {
        return true;
    }
    let bytes = key.as_bytes();
    let mut start = 0;
    for i in 0..=bytes.len() {
        let at_end = i == bytes.len();
        let sep = !at_end && matches!(bytes[i], b'_' | b'-' | b'.' | b' ');
        // camelCase: a lower letter followed by an upper one ends a word.
        let camel = !at_end
            && i > start
            && bytes[i].is_ascii_uppercase()
            && bytes[i - 1].is_ascii_lowercase();
        if at_end || sep || camel {
            if i > start && SECRET_WORDS.contains(&&lower[start..i]) {
                return true;
            }
            start = if camel { i } else { i + 1 };
        }
    }
    false
}

/// Values that are credentials whatever their key is called.
pub fn looks_secret(value: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "sk-",
        "sk_live_",
        "sk_test_",
        "rk_live_",
        "pk_live_",
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "ghr_",
        "github_pat_",
        "glpat-",
        "xoxb-",
        "xoxp-",
        "xoxa-",
        "xoxs-",
        "AKIA",
        "ASIA",
        "AIza",
        "SG.",
        "npm_",
        "pypi-",
        "hf_",
        "-----BEGIN",
    ];
    if PREFIXES
        .iter()
        .any(|prefix| value.starts_with(prefix) && value.len() >= prefix.len() + 8)
    {
        return true;
    }
    // A JWT: three base64url parts, the first starting with {"…
    if value.starts_with("eyJ") && value.matches('.').count() == 2 {
        return true;
    }
    // A URL with a password: scheme://user:pass@host
    if let Some(scheme) = value.find("://") {
        let rest = &value[scheme + 3..];
        if rest.find('@').is_some_and(|at| rest[..at].contains(':')) {
            return true;
        }
    }
    looks_random(value)
}

/// 24+ characters of letters and digits (plus `- _ + / =`), with both letters and digits and no
/// spaces: a key, not a word or a path.
fn looks_random(value: &str) -> bool {
    if value.len() < 24 {
        return false;
    }
    let (mut letters, mut digits, mut upper) = (0, 0, 0);
    for b in value.bytes() {
        if b.is_ascii_digit() {
            digits += 1;
        } else if b.is_ascii_alphabetic() {
            letters += 1;
            if b.is_ascii_uppercase() {
                upper += 1;
            }
        } else if !matches!(b, b'-' | b'_' | b'+' | b'/' | b'=') {
            return false;
        }
    }
    if digits < 3 || letters < 6 {
        return false;
    }
    // A plain hex string or a mixed-case token both count; a lower-case slug with a few digits
    // ("my-app-production-2024-…") doesn't.
    value.bytes().all(|b| b.is_ascii_hexdigit()) || upper >= 3
}

/// Token shapes that are secrets in any file, with a name for each.
static TOKENS: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
    [
        ("AWS access key", r"AKIA[0-9A-Z]{16}"),
        ("GitHub token", r"gh[pousr]_[A-Za-z0-9]{20,}"),
        ("GitHub token", r"github_pat_[A-Za-z0-9_]{20,}"),
        ("GitLab token", r"glpat-[A-Za-z0-9_\-]{20,}"),
        ("Slack token", r"xox[baprs]-[A-Za-z0-9\-]{10,}"),
        ("Stripe key", r"[rs]k_live_[A-Za-z0-9]{16,}"),
        ("Anthropic key", r"sk-ant-[A-Za-z0-9_\-]{20,}"),
        ("OpenAI key", r"sk-proj-[A-Za-z0-9_\-]{20,}"),
        ("Google API key", r"AIza[A-Za-z0-9_\-]{35}"),
        ("npm token", r"npm_[A-Za-z0-9]{36}"),
        (
            "private key",
            r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?(?:-----END [A-Z ]*PRIVATE KEY-----|\z)",
        ),
        (
            "JWT",
            r"eyJ[A-Za-z0-9_\-]{7,}\.eyJ[A-Za-z0-9_\-]{7,}\.[A-Za-z0-9_\-]+",
        ),
    ]
    .into_iter()
    .map(|(name, pattern)| (name, Regex::new(pattern).unwrap()))
    .collect()
});

/// The first well-known token in `text`: its kind and 1-based line.
pub fn find_token(text: &str) -> Option<(&'static str, usize)> {
    let (name, at) = TOKENS
        .iter()
        .filter_map(|(name, regex)| regex.find(text).map(|m| (*name, m.start())))
        .min_by_key(|&(_, at)| at)?;
    Some((name, text[..at].matches('\n').count() + 1))
}

/// Replaces every well-known token in `text` (API keys, access tokens, private key blocks,
/// JWTs) with [`REDACTED`].
pub fn redact_tokens(text: &str) -> Cow<'_, str> {
    let ranges = TOKENS
        .iter()
        .flat_map(|(_, regex)| regex.find_iter(text).map(|m| m.range()))
        .collect();
    replace_ranges(text, ranges)
}

/// Replaces the secrets in `text`, the whole contents of a file of the given kind (or of an
/// ordinary file, when `None`), with [`REDACTED`].
pub fn redact(text: &str, kind: Option<SecretFile>) -> Cow<'_, str> {
    let Some(kind) = kind else {
        return redact_tokens(text);
    };
    let mut ranges = Vec::new();
    let mut start = 0;
    for line in text.split_inclusive('\n') {
        let content = line.trim_end_matches(['\n', '\r']);
        if let Some(range) = line_secret(kind, content) {
            ranges.push(start + range.start..start + range.end);
        }
        start += line.len();
    }
    let masked = replace_ranges(text, ranges);
    match redact_tokens(&masked) {
        Cow::Borrowed(_) => masked,
        Cow::Owned(owned) => Cow::Owned(owned),
    }
}

/// Replaces byte `ranges` of `text` with [`REDACTED`], merging any that overlap.
fn replace_ranges(text: &str, mut ranges: Vec<Range<usize>>) -> Cow<'_, str> {
    if ranges.is_empty() {
        return Cow::Borrowed(text);
    }
    ranges.sort_by_key(|range| range.start);
    let mut out = String::with_capacity(text.len());
    let mut pos = 0;
    for range in ranges {
        if range.end <= pos {
            continue;
        }
        out.push_str(&text[pos..range.start.max(pos)]);
        out.push_str(REDACTED);
        pos = range.end;
    }
    out.push_str(&text[pos..]);
    Cow::Owned(out)
}

#[cfg(test)]
mod test {
    use super::*;

    // Every credential below is a made-up value of the right shape.

    #[test]
    fn keys() {
        for key in [
            "API_KEY",
            "OPENAI_API_KEY",
            "apiKey",
            "STRIPE_SECRET",
            "DB_PASSWORD",
            "DBPASSWORD",
            "GITHUB_TOKEN",
            "JWT_SIGNING_KEY",
            "SESSION_SECRET",
            "DATABASE_URL",
            "AWS_ACCESS_KEY_ID",
            "clientSecret",
            "PRIVATE_KEY",
            "SLACK_WEBHOOK",
            "MYSQL_PWD",
            "SENTRY_DSN",
        ] {
            assert!(is_secret_key(key), "{key}");
        }
        for key in [
            "PORT",
            "NODE_ENV",
            "RAPID_MODE",
            "APP_NAME",
            "LOG_LEVEL",
            "HOSTNAME",
            "MONKEY_COUNT",
            "KEYBOARD_LAYOUT",
            "SESSION_TTL",
            "CONNECTION_TIMEOUT",
            "CERT_PATH",
            "ACCESS_LOG",
        ] {
            assert!(!is_secret_key(key), "{key}");
        }
    }

    #[test]
    fn values() {
        assert!(looks_secret("sk-proj-abcdefghijklmnop"));
        assert!(looks_secret("ghp_0123456789abcdefABCDEF"));
        assert!(looks_secret("AKIAIOSFODNN7EXAMPLE"));
        assert!(looks_secret("eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.sig"));
        assert!(looks_secret("postgres://app:hunter2@db:5432/app"));
        assert!(looks_secret("9f86d081884c7d659a2feaa0c55ad015a3bf4f1b"));
        assert!(looks_secret("Zx81Kq0PwL2mN7vB4cR9tY6u"));
        assert!(!looks_secret("https://example.com/path"));
        assert!(!looks_secret("production"));
        assert!(!looks_secret("my-app-production-2024-west"));
        assert!(!looks_secret("C:/Users/me/projects/nib/out"));
    }

    #[test]
    fn env_lines() {
        let line = "export API_KEY = \"abc\" ";
        assert_eq!(env_line_secret(line), Some(17..22));
        assert_eq!(env_line_secret("PORT=8080 # web"), None);
        assert_eq!(env_line_secret("TOKEN=x1"), Some(6..8));
        assert_eq!(env_line_secret("  # API_KEY=abc"), None);
        assert_eq!(env_line_secret("SECRET=x # note"), Some(7..8));
        assert_eq!(env_line_secret("EMPTY_TOKEN="), None);
        assert_eq!(env_line_secret("PLAIN"), None);
        let bare = "ghp_0123456789abcdefABCDEF";
        assert_eq!(env_line_secret(bare), Some(0..bare.len()));
        assert!(is_env_file(".env.local"));
        assert!(is_env_file("prod.env"));
        assert!(!is_env_file("environment.ts"));
    }

    #[test]
    fn npmrc() {
        let token = "//registry.npmjs.org/:_authToken=abc123";
        assert_eq!(env_line_secret(token), Some(33..token.len()));
        assert!(env_line_secret("//npm.example.com/:_auth = \"dXNlcjpwYXNz\"").is_some());
        assert_eq!(
            env_line_secret("registry=https://registry.npmjs.org/"),
            None
        );
        assert_eq!(env_line_secret("API_KEY: x=1"), Some(9..12));
        assert!(is_env_file(".npmrc"));
    }

    #[test]
    fn secret_files() {
        for name in [
            "id_rsa",
            "server.pem",
            "tls.key",
            "passwords.txt",
            "api-token",
            "prod.secret",
            "encryption.txt",
            "my_keys.txt",
            "recovery-codes.txt",
            "dbpassword",
            "credentials",
            ".netrc",
            "seed phrase.txt",
        ] {
            assert!(is_secret_file(name), "{name}");
        }
        for name in [
            "auth.go",
            "token.rs",
            "keyboard.txt",
            "notes.txt",
            "readme",
            "id_rsa.pub",
            "api.ts",
            "keys.json",
            "monkey.txt",
            "license",
        ] {
            assert!(!is_secret_file(name), "{name}");
        }
        assert_eq!(whole_line_secret("  hunter2 "), Some(2..9));
        assert_eq!(whole_line_secret("   "), None);
        assert_eq!(classify(Path::new("/srv/app/.env")), Some(SecretFile::Env));
        assert_eq!(classify(Path::new("keys/tls.KEY")), Some(SecretFile::Whole));
        assert_eq!(classify(Path::new("src/main.rs")), None);
    }

    #[test]
    fn tokens() {
        let text = "fn main() {\n    let key = \"ghp_0123456789abcdefABCDEF\";\n}\n";
        assert_eq!(find_token(text), Some(("GitHub token", 2)));
        assert_eq!(
            redact_tokens(text),
            "fn main() {\n    let key = \"[REDACTED]\";\n}\n"
        );
        assert_eq!(find_token("no secrets here"), None);
        assert!(matches!(redact_tokens("plain text"), Cow::Borrowed(_)));
        let pem = "a\n-----BEGIN RSA PRIVATE KEY-----\nMIIfake\n-----END RSA PRIVATE KEY-----\nb";
        assert_eq!(redact_tokens(pem), "a\n[REDACTED]\nb");
        assert_eq!(
            redact_tokens("aws AKIAIOSFODNN7EXAMPLE and AKIAIOSFODNN7EXAMPL"),
            "aws [REDACTED] and AKIAIOSFODNN7EXAMPL"
        );
    }

    #[test]
    fn redacts_files() {
        let env = "PORT=8080\nAPI_KEY=abc123 # dev\r\n# TOKEN=x\nexport DB_PASSWORD='hunter2'\n";
        assert_eq!(
            redact(env, Some(SecretFile::Env)),
            "PORT=8080\nAPI_KEY=[REDACTED] # dev\r\n# TOKEN=x\nexport DB_PASSWORD=[REDACTED]\n"
        );
        assert_eq!(
            redact("  hunter2\n\nswordfish", Some(SecretFile::Whole)),
            "  [REDACTED]\n\n[REDACTED]"
        );
        assert_eq!(redact("PORT=8080", None), "PORT=8080");
    }
}
