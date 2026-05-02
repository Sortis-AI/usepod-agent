//! Ed25519 identity keypair: generate-on-first-run, persist with mode 0600,
//! idempotent reload. See `plan/V2_AGENT_SPEC.md` §4.1.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use chrono::{DateTime, Utc};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use rand_core::OsRng;

const FORMAT_VERSION: u32 = 1;
const HEADER: &str = "-----BEGIN USEPOD AGENT KEY-----";
const FOOTER: &str = "-----END USEPOD AGENT KEY-----";

#[derive(Debug)]
pub struct Identity {
    pub path: PathBuf,
    pub created: DateTime<Utc>,
    pub provider_id: Option<String>,
    signing_key: SigningKey,
    verifying_key: VerifyingKey,
}

impl Identity {
    pub fn public_key_b64(&self) -> String {
        B64.encode(self.verifying_key.as_bytes())
    }

    pub fn sign_b64(&self, message: &[u8]) -> String {
        let sig: Signature = self.signing_key.sign(message);
        B64.encode(sig.to_bytes())
    }

    /// Persist `provider_id` to the identity file after a successful enrollment.
    pub fn set_provider_id(&mut self, provider_id: String) -> Result<()> {
        self.provider_id = Some(provider_id);
        let body = serialize(
            self.created,
            self.signing_key.to_bytes(),
            self.verifying_key.to_bytes(),
            self.provider_id.as_deref(),
        );
        write_with_mode_0600(&self.path, &body)
    }
}

/// Load the identity from disk, generating + persisting a new one on first run.
pub fn load_or_create(path: &Path) -> Result<Identity> {
    if path.exists() {
        return load(path);
    }
    create_new(path)
}

fn create_new(path: &Path) -> Result<Identity> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating identity dir {}", parent.display()))?;
    }
    let signing_key = SigningKey::generate(&mut OsRng);
    let verifying_key = signing_key.verifying_key();
    let created = Utc::now();
    let body = serialize(
        created,
        signing_key.to_bytes(),
        verifying_key.to_bytes(),
        None,
    );
    write_with_mode_0600(path, &body)?;
    tracing::info!(path = %path.display(), "generated new agent identity");
    Ok(Identity {
        path: path.to_path_buf(),
        created,
        provider_id: None,
        signing_key,
        verifying_key,
    })
}

fn load(path: &Path) -> Result<Identity> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading identity {}", path.display()))?;
    let parsed = parse(&raw).with_context(|| format!("parsing identity {}", path.display()))?;

    let secret_bytes = B64
        .decode(parsed.secret_b64.as_bytes())
        .context("decoding base64 secret")?;
    let secret_arr: [u8; 32] = secret_bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow!("secret key must decode to 32 bytes"))?;
    let signing_key = SigningKey::from_bytes(&secret_arr);
    let verifying_key = signing_key.verifying_key();

    // Sanity-check the stored public key matches.
    let recorded_pub = B64
        .decode(parsed.public_b64.as_bytes())
        .context("decoding base64 public")?;
    if recorded_pub.as_slice() != verifying_key.as_bytes() {
        bail!("identity file public key does not match secret key; refusing to load");
    }

    warn_if_world_readable(path);

    Ok(Identity {
        path: path.to_path_buf(),
        created: parsed.created,
        provider_id: parsed.provider_id,
        signing_key,
        verifying_key,
    })
}

struct Parsed {
    created: DateTime<Utc>,
    secret_b64: String,
    public_b64: String,
    provider_id: Option<String>,
}

fn parse(raw: &str) -> Result<Parsed> {
    let lines: Vec<&str> = raw.lines().collect();
    let start = lines
        .iter()
        .position(|l| l.trim() == HEADER)
        .ok_or_else(|| anyhow!("missing header {HEADER}"))?;
    let end = lines
        .iter()
        .position(|l| l.trim() == FOOTER)
        .ok_or_else(|| anyhow!("missing footer {FOOTER}"))?;
    if end <= start {
        bail!("malformed identity file: footer before header");
    }

    let mut version: Option<u32> = None;
    let mut created: Option<DateTime<Utc>> = None;
    let mut secret: Option<String> = None;
    let mut public: Option<String> = None;
    let mut provider_id: Option<String> = None;

    for line in &lines[start + 1..end] {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let (k, v) = trimmed
            .split_once(':')
            .ok_or_else(|| anyhow!("malformed line: {trimmed}"))?;
        let v = v.trim().to_string();
        match k.trim() {
            "version" => version = Some(v.parse()?),
            "created" => created = Some(v.parse::<DateTime<Utc>>()?),
            "secret" => secret = Some(v),
            "public" => public = Some(v),
            "provider_id" => provider_id = Some(v),
            _ => {} // forward-compat
        }
    }

    let version = version.ok_or_else(|| anyhow!("missing `version`"))?;
    if version != FORMAT_VERSION {
        bail!("unsupported identity format version {version}");
    }
    Ok(Parsed {
        created: created.ok_or_else(|| anyhow!("missing `created`"))?,
        secret_b64: secret.ok_or_else(|| anyhow!("missing `secret`"))?,
        public_b64: public.ok_or_else(|| anyhow!("missing `public`"))?,
        provider_id,
    })
}

fn serialize(
    created: DateTime<Utc>,
    secret: [u8; 32],
    public: [u8; 32],
    provider_id: Option<&str>,
) -> String {
    let mut out = String::new();
    out.push_str(HEADER);
    out.push('\n');
    out.push_str(&format!("version: {FORMAT_VERSION}\n"));
    out.push_str(&format!("created: {}\n", created.to_rfc3339()));
    out.push_str(&format!("secret: {}\n", B64.encode(secret)));
    out.push_str(&format!("public: {}\n", B64.encode(public)));
    if let Some(pid) = provider_id {
        out.push_str(&format!("provider_id: {pid}\n"));
    }
    out.push_str(FOOTER);
    out.push('\n');
    out
}

fn write_with_mode_0600(path: &Path, body: &str) -> Result<()> {
    std::fs::write(path, body)
        .with_context(|| format!("writing identity {}", path.display()))?;
    set_owner_only_perms(path)?;
    Ok(())
}

#[cfg(unix)]
fn set_owner_only_perms(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(path, perms)
        .with_context(|| format!("setting 0600 on {}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_owner_only_perms(_path: &Path) -> Result<()> {
    // On Windows the file inherits ACLs from the parent (typically the user's profile).
    // No equivalent action needed here.
    Ok(())
}

#[cfg(unix)]
fn warn_if_world_readable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            tracing::warn!(
                path = %path.display(),
                mode = format!("{mode:o}"),
                "identity file is accessible to other users; chmod 600 it"
            );
        }
    }
}

#[cfg(not(unix))]
fn warn_if_world_readable(_path: &Path) {}
