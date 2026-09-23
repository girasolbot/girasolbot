//! API-key vault — local, encrypted-at-rest storage for the key-management
//! panel. SHARED with the TS bot (robinhood-bot-aws): both bots read and
//! write the SAME `<DATA_DIR>/apikeys.enc.json` in the unified vault v2
//! format, so one file + one `VAULT_PASSPHRASE` serves both bots.
//!
//! Design:
//!   - Keys are stored ONLY in `<DATA_DIR>/apikeys.enc.json`, never in git
//!     (the file lives inside the gitignored `data/` directory).
//!   - At-rest encryption: AES-256-GCM. The AES key is derived with scrypt
//!     (N=16384, r=8, p=1, 32-byte output) from the passphrase and the
//!     per-file random salt: the `VAULT_PASSPHRASE` env var when set,
//!     otherwise a per-installation random secret persisted (0600) beside
//!     the vault. This protects against casual disk reads / accidental
//!     commits; it is not a defense against a compromised host.
//!   - Values are readable only through `get_api_key()` and never logged.
//!   - The panel API surface: set/delete/list (masked) per slot.
//!
//! Vault formats:
//!   - v2 (written): `{ version: 2, kdf: "scrypt", kdf_params: {N,r,p},
//!     salt: <hex32B>, slots: { "<slot_id>": { iv, ct, tag } } }`
//!   - v1 (read-only, migrated to v2 on the first write): the Rust legacy
//!     layout `{ salts: {slot: salt_hex}, data: {slot: "iv:tag:ct"} }` with
//!     the SHA256-stretched KDF, and the TS legacy layout
//!     `{ salt: <hex>, data: {slot: "iv:tag:ct"} }` with plain scrypt.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use scrypt::{scrypt, Params as ScryptParams};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use std::collections::HashMap;
use std::path::PathBuf;

/// Which bot network(s) a slot belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SlotNetwork {
    /// EVM copytrading bot (robinhood-bot-aws).
    Evm,
    /// Solana bot (this repo).
    Solana,
    /// Shared by both bots — one slot, not duplicated per network.
    Both,
}

impl SlotNetwork {
    pub fn as_str(&self) -> &'static str {
        match self {
            SlotNetwork::Evm => "evm",
            SlotNetwork::Solana => "solana",
            SlotNetwork::Both => "both",
        }
    }
}

/// One configurable service endpoint either bot may call.
#[derive(Clone, Debug)]
pub struct ApiKeySlot {
    /// Stable slot id used by the panel API and the vault file.
    pub id: &'static str,
    /// Human-readable service name.
    pub label: &'static str,
    /// What the key is used for inside the bot.
    pub purpose: &'static str,
    /// Where users typically obtain a key.
    pub signup_url: &'static str,
    /// Pricing model of the service.
    pub pricing: Pricing,
    /// Settings field the bot reads as a fallback (vault keys take precedence).
    pub env_var: &'static str,
    /// Example key shape for UI validation hints.
    pub placeholder: &'static str,
    /// Bot network(s) the slot serves: evm, solana or both.
    pub network: SlotNetwork,
}

/// Pricing model of a provider.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Pricing {
    /// Fully free tier.
    Free,
    /// Free tier paid for by optional tips / referrals — highlighted first.
    Tips,
    /// Freemium: free quota + paid tiers.
    Freemium,
    /// Subscription required.
    Subscription,
}

impl Pricing {
    pub fn as_str(&self) -> &'static str {
        match self {
            Pricing::Free => "free",
            Pricing::Tips => "tips",
            Pricing::Freemium => "freemium",
            Pricing::Subscription => "subscription",
        }
    }
}

/// Unified service catalog shared with the TS bot (robinhood-bot-aws).
/// `tips` providers are free (paid for by optional tips on transactions) and
/// are highlighted first in the panel and README. `helius` is a single
/// shared slot (network "both") — do not duplicate it per network.
pub const API_KEY_SLOTS: &[ApiKeySlot] = &[
    // --- Solana (this bot) slots ---
    ApiKeySlot {
        id: "rpc-http",
        label: "RPC (HTTP)",
        purpose: "JSON-RPC reads, getTransaction, price/liquidity lookups (config solana_rpc_urls)",
        signup_url: "https://solana.com/rpc",
        pricing: Pricing::Free,
        env_var: "SNIPER_RPC_URLS",
        placeholder: "https://<provider>/rpc/<key>",
        network: SlotNetwork::Solana,
    },
    ApiKeySlot {
        id: "rpc-wss",
        label: "RPC WSS feed",
        purpose: "logsSubscribe websocket feed for target-wallet detection (config solana_ws_urls)",
        signup_url: "https://solana.com/rpc",
        pricing: Pricing::Free,
        env_var: "SNIPER_WSS_URLS",
        placeholder: "wss://<provider>/rpc/<key>",
        network: SlotNetwork::Solana,
    },
    ApiKeySlot {
        id: "erpc",
        label: "ERPC (edge RPC)",
        purpose: "Low-latency edge RPC / WSS endpoints with api-key auth (config solana_rpc_urls / solana_ws_urls)",
        signup_url: "https://erpc.global",
        pricing: Pricing::Freemium,
        env_var: "ERPC_API_KEY",
        placeholder: "<api-key token>",
        network: SlotNetwork::Solana,
    },
    ApiKeySlot {
        id: "geyser",
        label: "Yellowstone gRPC Geyser",
        purpose: "Direct gRPC transaction stream, pre-confirmation (config geyser_url)",
        signup_url: "https://erpc.global",
        pricing: Pricing::Freemium,
        env_var: "SNIPER_GEYSER_URL",
        placeholder: "http(s)://<geyser-host>:<port>",
        network: SlotNetwork::Solana,
    },
    ApiKeySlot {
        id: "shreds",
        label: "ShredStream",
        purpose: "Raw shred stream before block formation (config shreds_url)",
        signup_url: "https://erpc.global",
        pricing: Pricing::Freemium,
        env_var: "SNIPER_SHREDS_URL",
        placeholder: "http(s)://<shredstream-host>:<port>",
        network: SlotNetwork::Solana,
    },
    ApiKeySlot {
        id: "jito",
        label: "Jito Block Engine",
        purpose: "SWQoS submission via block engine (config swqos_jito_url)",
        signup_url: "https://jito.wtf",
        pricing: Pricing::Tips,
        env_var: "SNIPER_JITO_URL",
        placeholder: "https://<region>.mainnet.block-engine.jito.wtf",
        network: SlotNetwork::Solana,
    },
    ApiKeySlot {
        id: "bloxroute",
        label: "bloXroute",
        purpose: "QUIC/HTTP SWQoS submission (config bloxroute_api_key)",
        signup_url: "https://bloxroute.com",
        pricing: Pricing::Subscription,
        env_var: "BLOXROUTE_API_KEY",
        placeholder: "<api-token>",
        network: SlotNetwork::Solana,
    },
    ApiKeySlot {
        id: "nextblock",
        label: "NextBlock",
        purpose: "QUIC SWQoS submission (config nextblock_api_key)",
        signup_url: "https://nextblock.io",
        pricing: Pricing::Tips,
        env_var: "NEXTBLOCK_API_KEY",
        placeholder: "<api-key>",
        network: SlotNetwork::Solana,
    },
    ApiKeySlot {
        id: "zeroslot",
        label: "ZeroSlot",
        purpose: "SWQoS submission (config zeroslot_api_key)",
        signup_url: "https://0slot.trade",
        pricing: Pricing::Tips,
        env_var: "ZEROSLOT_API_KEY",
        placeholder: "<api-key>",
        network: SlotNetwork::Solana,
    },
    ApiKeySlot {
        id: "nozomi",
        label: "Nozomi (Temporal)",
        purpose: "SWQoS submission (config nozomi_api_key)",
        signup_url: "https://temporal.xyz",
        pricing: Pricing::Tips,
        env_var: "NOZOMI_API_KEY",
        placeholder: "<api-key>",
        network: SlotNetwork::Solana,
    },
    // --- EVM (TS bot) slots, stored in the same shared vault file ---
    ApiKeySlot {
        id: "zerox",
        label: "0x Swap API",
        purpose: "Swap-quote fallback when the primary router path reverts (EVM bot, config ZEROX_API_KEY)",
        signup_url: "https://0x.org",
        pricing: Pricing::Freemium,
        env_var: "ZEROX_API_KEY",
        placeholder: "0x API key (empty = free unauthenticated tier)",
        network: SlotNetwork::Evm,
    },
    ApiKeySlot {
        id: "dexscreener",
        label: "DexScreener",
        purpose: "Optional token metadata / pair enrichment, public endpoints need no key today (EVM bot)",
        signup_url: "https://docs.dexscreener.com",
        pricing: Pricing::Free,
        env_var: "DEXSCREENER_API_KEY",
        placeholder: "empty = use unauthenticated public endpoints",
        network: SlotNetwork::Evm,
    },
    ApiKeySlot {
        id: "coingecko",
        label: "CoinGecko",
        purpose: "Optional price/metadata lookups, public demo tier available (EVM bot)",
        signup_url: "https://www.coingecko.com/en/api",
        pricing: Pricing::Freemium,
        env_var: "COINGECKO_API_KEY",
        placeholder: "CG-<key> (empty = public demo tier)",
        network: SlotNetwork::Evm,
    },
    ApiKeySlot {
        id: "alchemy",
        label: "Alchemy",
        purpose: "Optional archive/enhanced RPC for backfill and route history (EVM bot)",
        signup_url: "https://alchemy.com",
        pricing: Pricing::Freemium,
        env_var: "ALCHEMY_API_KEY",
        placeholder: "https://<chain>.g.alchemy.com/v2/<key>",
        network: SlotNetwork::Evm,
    },
    ApiKeySlot {
        id: "quicknode",
        label: "QuickNode",
        purpose: "Optional dedicated RPC endpoint (EVM bot)",
        signup_url: "https://quicknode.com",
        pricing: Pricing::Freemium,
        env_var: "QUICKNODE_URL",
        placeholder: "https://<name>.<chain>.quiknode.pro/<token>/",
        network: SlotNetwork::Evm,
    },
    ApiKeySlot {
        id: "infura",
        label: "Infura",
        purpose: "Optional backup RPC provider (EVM bot)",
        signup_url: "https://infura.io",
        pricing: Pricing::Freemium,
        env_var: "INFURA_URL",
        placeholder: "https://<network>.infura.io/v3/<key>",
        network: SlotNetwork::Evm,
    },
    ApiKeySlot {
        id: "explorer",
        label: "Explorer API",
        purpose: "Optional Blockscout/explorer key for higher rate limits (EVM bot)",
        signup_url: "https://robinhoodchain.blockscout.com",
        pricing: Pricing::Free,
        env_var: "EXPLORER_API_KEY",
        placeholder: "empty = public rate limits",
        network: SlotNetwork::Evm,
    },
    // --- Shared between both bots (single slot, not duplicated per network) ---
    ApiKeySlot {
        id: "helius",
        label: "Helius",
        purpose: "Shared slot: Solana sender/SWQoS + enhanced RPC (this bot) and EVM enhanced-index integrations (TS bot)",
        signup_url: "https://helius.dev",
        pricing: Pricing::Freemium,
        env_var: "HELIUS_API_KEY",
        placeholder: "<api-key>",
        network: SlotNetwork::Both,
    },
];

/// Find a slot by id.
pub fn find_slot(id: &str) -> Option<&'static ApiKeySlot> {
    API_KEY_SLOTS.iter().find(|s| s.id == id)
}

/// Masked view returned to the panel — never a raw key.
#[derive(Serialize, Clone, Debug)]
pub struct ApiKeySlotStatus {
    pub id: String,
    pub label: String,
    pub purpose: String,
    pub signup_url: String,
    pub pricing: String,
    pub env_var: String,
    pub placeholder: String,
    /// Bot network(s) the slot serves: "evm", "solana" or "both".
    pub network: String,
    /// True when a key/URL is configured (vault or settings fallback).
    pub configured: bool,
    /// Masked preview like `sk-…f3ab` — safe to display.
    pub masked: Option<String>,
    /// Where the active value comes from.
    pub source: Option<&'static str>,
}

/// Unified vault format version written by both bots.
pub const VAULT_VERSION_V2: u32 = 2;
/// Legacy format version (pre-unification).
pub const VAULT_VERSION_V1: u32 = 1;

/// scrypt parameters — fixed by the unified v2 spec (both bots must agree).
pub const VAULT_KDF_N: u32 = 16384;
pub const VAULT_KDF_R: u32 = 8;
pub const VAULT_KDF_P: u32 = 1;

/// One encrypted slot entry of the unified v2 format.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SlotBlobV2 {
    /// 12-byte AES-GCM nonce, hex.
    pub iv: String,
    /// Ciphertext (without the auth tag), hex.
    pub ct: String,
    /// 16-byte AES-GCM auth tag, hex.
    pub tag: String,
}

/// scrypt parameters of the unified v2 format.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ScryptParamsV2 {
    pub N: u32,
    pub r: u32,
    pub p: u32,
}

impl Default for ScryptParamsV2 {
    fn default() -> Self {
        ScryptParamsV2 { N: VAULT_KDF_N, r: VAULT_KDF_R, p: VAULT_KDF_P }
    }
}

/// Unified vault file layout (version 2, written and read by both bots).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct VaultFileV2 {
    pub version: u32,
    /// Only "scrypt" in v2.
    pub kdf: String,
    pub kdf_params: ScryptParamsV2,
    /// scrypt salt, hex (32 random bytes).
    pub salt: String,
    /// slot id → encrypted blob.
    pub slots: HashMap<String, SlotBlobV2>,
}

/// Legacy v1 vault (Rust writer): per-slot salts + "iv:tag:ct" blobs.
#[derive(Serialize, Deserialize, Clone, Debug)]
struct VaultFileV1Rust {
    version: u32,
    /// Per-slot salt hex (unique per entry).
    salts: HashMap<String, String>,
    /// Per-slot encrypted blob: `iv_hex:tag_hex:ciphertext_hex`.
    data: HashMap<String, String>,
}

/// Legacy v1 vault (TS writer): single salt + "iv:tag:ct" blobs.
#[derive(Serialize, Deserialize, Clone, Debug)]
struct VaultFileV1Ts {
    version: u32,
    /// scrypt salt, hex.
    salt: String,
    /// slot → "iv:tag:ct" hex blob.
    data: HashMap<String, String>,
}

/// Parsed vault file in any supported layout (v1-Rust, v1-TS or v2).
#[derive(Clone, Debug)]
enum VaultFileAny {
    V2(VaultFileV2),
    V1Rust(VaultFileV1Rust),
    V1Ts(VaultFileV1Ts),
}

pub struct ApiKeyVault {
    dir: PathBuf,
    io_lock: Mutex<()>,
}

impl ApiKeyVault {
    pub fn new(dir: PathBuf) -> Self {
        ApiKeyVault { dir, io_lock: Mutex::new(()) }
    }

    fn vault_path(&self) -> PathBuf {
        self.dir.join("apikeys.enc.json")
    }

    fn secret_path(&self) -> PathBuf {
        self.dir.join(".apikeys-secret")
    }

    /// Load the vault secret: VAULT_PASSPHRASE env, else per-install random
    /// secret generated on first use and stored with 0600 permissions.
    fn load_secret(&self) -> Result<String, String> {
        if let Ok(p) = std::env::var("VAULT_PASSPHRASE") {
            if !p.trim().is_empty() {
                return Ok(p.trim().to_string());
            }
        }
        let path = self.secret_path();
        if let Ok(existing) = std::fs::read_to_string(&path) {
            let s = existing.trim().to_string();
            if !s.is_empty() {
                return Ok(s);
            }
        }
        std::fs::create_dir_all(&self.dir)
            .map_err(|e| format!("cannot create vault dir {}: {}", self.dir.display(), e))?;
        let secret = random_hex(32);
        std::fs::write(&path, &secret)
            .map_err(|e| format!("cannot write vault secret: {}", e))?;
        restrict_permissions(&path);
        log::info!("apikeys: generated per-installation vault secret (0600)");
        Ok(secret)
    }

    /// Parse raw JSON into a tagged vault layout. Anything that does not
    /// match a known layout is an error (the caller decides what to do).
    fn parse(text: &str) -> Result<VaultFileAny, String> {
        let raw: Value = serde_json::from_str(text)
            .map_err(|e| format!("vault json parse failed: {}", e))?;
        let version = raw.get("version").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        if version == VAULT_VERSION_V2 {
            let v: VaultFileV2 = serde_json::from_value(raw)
                .map_err(|e| format!("vault v2 structure invalid: {}", e))?;
            if v.kdf != "scrypt" {
                return Err(format!("unsupported vault kdf: {}", v.kdf));
            }
            if v.kdf_params != ScryptParamsV2::default() {
                return Err("unsupported scrypt params".to_string());
            }
            return Ok(VaultFileAny::V2(v));
        }
        if version == VAULT_VERSION_V1 {
            // Rust v1 layout: per-slot `salts`. Checked FIRST — both v1
            // layouts carry a `data` object.
            if raw.get("salts").map(|s| s.is_object()).unwrap_or(false) {
                let v: VaultFileV1Rust = serde_json::from_value(raw)
                    .map_err(|e| format!("vault v1(rust) structure invalid: {}", e))?;
                return Ok(VaultFileAny::V1Rust(v));
            }
            if raw.get("salt").map(|s| s.is_string()).unwrap_or(false) {
                let v: VaultFileV1Ts = serde_json::from_value(raw)
                    .map_err(|e| format!("vault v1(ts) structure invalid: {}", e))?;
                return Ok(VaultFileAny::V1Ts(v));
            }
        }
        Err("unsupported vault version".to_string())
    }

    fn empty_v2() -> VaultFileV2 {
        VaultFileV2 {
            version: VAULT_VERSION_V2,
            kdf: "scrypt".to_string(),
            kdf_params: ScryptParamsV2::default(),
            salt: random_hex(32),
            slots: HashMap::new(),
        }
    }

    fn load(&self) -> VaultFileAny {
        let path = self.vault_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => match Self::parse(&text) {
                Ok(v) => v,
                Err(e) => {
                    log::warn!("apikeys: vault file unreadable or unsupported version — starting empty ({}, {})", path.display(), e);
                    VaultFileAny::V2(Self::empty_v2())
                }
            },
            Err(_) => VaultFileAny::V2(Self::empty_v2()),
        }
    }

    /// Load the vault and migrate any legacy (v1) layout to the unified v2
    /// format, re-encrypting every legacy slot under the v2 scrypt KDF. The
    /// returned value is always a v2 file (not yet written to disk — callers
    /// persist it as part of their set/delete save).
    fn load_or_migrate(&self, secret: &str) -> VaultFileV2 {
        match self.load() {
            VaultFileAny::V2(v) => v,
            VaultFileAny::V1Rust(v1) => {
                let mut migrated = Self::empty_v2();
                for (slot, blob) in &v1.data {
                    let salt_hex = match v1.salts.get(slot) {
                        Some(s) => s,
                        None => continue,
                    };
                    if let Ok(salt) = hex::decode(salt_hex) {
                        // Rust v1 KDF: SHA256("girasol-vault-v1"+secret+salt)
                        // stretched by 4096 rounds of SHA256(prev || i_le).
                        if let Some(plain) = Self::decrypt_legacy_sha256(secret, blob, &salt) {
                            if let Ok(blob_v2) = Self::encrypt_v2(secret, &plain, &migrated.salt) {
                                migrated.slots.insert(slot.clone(), blob_v2);
                            }
                        }
                    }
                }
                migrated
            }
            VaultFileAny::V1Ts(v1) => {
                // TS v1 layout: single file salt, plain scrypt KDF.
                let mut migrated = Self::empty_v2();
                for (slot, blob) in &v1.data {
                    if let Some(plain) = Self::decrypt_legacy_scrypt(secret, blob, &v1.salt) {
                        if let Ok(blob_v2) = Self::encrypt_v2(secret, &plain, &migrated.salt) {
                            migrated.slots.insert(slot.clone(), blob_v2);
                        }
                    }
                }
                migrated
            }
        }
    }

    fn save(&self, v: &VaultFileV2) -> Result<(), String> {
        std::fs::create_dir_all(&self.dir)
            .map_err(|e| format!("cannot create vault dir: {}", e))?;
        let path = self.vault_path();
        let tmp = self.dir.join("apikeys.enc.json.tmp");
        let json = serde_json::to_string_pretty(v)
            .map_err(|e| format!("vault serialize failed: {}", e))?;
        std::fs::write(&tmp, json.as_bytes())
            .map_err(|e| format!("vault write failed: {}", e))?;
        restrict_permissions(&tmp);
        std::fs::rename(&tmp, &path)
            .map_err(|e| format!("vault rename failed: {}", e))?;
        restrict_permissions(&path);
        Ok(())
    }

    /// Unified v2 KDF: scrypt(passphrase, salt, 32) with the fixed spec
    /// parameters (N=16384, r=8, p=1) — identical to node:crypto
    /// `scryptSync(passphrase, salt, 32, {N, r, p})` used by the TS bot.
    fn derive_key_v2(secret: &str, salt_hex: &str) -> Result<[u8; 32], String> {
        let salt = hex::decode(salt_hex)
            .map_err(|_| "malformed vault salt".to_string())?;
        Self::scrypt_32(secret.as_bytes(), &salt)
    }

    /// scrypt(N=16384, r=8, p=1) → 32 bytes, the unified KDF.
    fn scrypt_32(passphrase: &[u8], salt: &[u8]) -> Result<[u8; 32], String> {
        // scrypt crate 0.11 takes log_n (N = 2^log_n); 16384 = 2^14.
        let params = ScryptParams::new(14, VAULT_KDF_R, VAULT_KDF_P, 32)
            .map_err(|_| "invalid scrypt params".to_string())?;
        let mut key = [0u8; 32];
        scrypt(passphrase, salt, &params, &mut key);
        Ok(key)
    }

    /// v2-encrypt a plaintext under the file-level salt. Returns the
    /// structured {iv, ct, tag} entry.
    fn encrypt_v2(secret: &str, plain: &str, salt_hex: &str) -> Result<SlotBlobV2, String> {
        let key_bytes = Self::derive_key_v2(secret, salt_hex)?;
        let key = Key::<Aes256Gcm>::from_slice(&key_bytes);
        let cipher = Aes256Gcm::new(key);
        let iv_bytes = random_bytes(12);
        let nonce = Nonce::from_slice(&iv_bytes);
        let ct = cipher
            .encrypt(nonce, plain.as_bytes())
            .map_err(|_| "vault encrypt failed".to_string())?;
        // AES-GCM: last 16 bytes of ct are the auth tag
        let (ct_body, tag) = ct.split_at(ct.len() - 16);
        Ok(SlotBlobV2 {
            iv: hex::encode(&iv_bytes),
            ct: hex::encode(ct_body),
            tag: hex::encode(tag),
        })
    }

    /// v2-decrypt a structured {iv, ct, tag} entry under the file salt.
    fn decrypt_v2(secret: &str, blob: &SlotBlobV2, salt_hex: &str) -> Result<String, String> {
        let key_bytes = Self::derive_key_v2(secret, salt_hex)?;
        let key = Key::<Aes256Gcm>::from_slice(&key_bytes);
        let cipher = Aes256Gcm::new(key);
        let iv = hex::decode(&blob.iv).map_err(|_| "malformed iv".to_string())?;
        let tag = hex::decode(&blob.tag).map_err(|_| "malformed tag".to_string())?;
        let ct = hex::decode(&blob.ct).map_err(|_| "malformed ciphertext".to_string())?;
        let mut full = ct;
        full.extend_from_slice(&tag);
        let nonce = Nonce::from_slice(&iv);
        let plain = cipher
            .decrypt(nonce, full.as_ref())
            .map_err(|_| "vault decrypt failed (wrong passphrase?)".to_string())?;
        String::from_utf8(plain).map_err(|_| "vault plaintext not utf8".to_string())
    }

    /// Legacy Rust v1 KDF: SHA256("girasol-vault-v1"+secret+salt) stretched
    /// by 4096 rounds of SHA256(prev || i_le) → 32-byte AES key.
    fn derive_key_v1_rust(secret: &str, salt: &[u8]) -> [u8; 32] {
        let mut block = {
            let mut hasher = Sha256::new();
            hasher.update(b"girasol-vault-v1");
            hasher.update(secret.as_bytes());
            hasher.update(salt);
            hasher.finalize()
        };
        for i in 0..4096u32 {
            let mut hasher = Sha256::new();
            hasher.update(block);
            hasher.update(i.to_le_bytes());
            block = hasher.finalize();
        }
        let mut key = [0u8; 32];
        key.copy_from_slice(&block);
        key
    }

    /// Legacy v1 AES-GCM decrypt over a "iv:tag:ct" hex blob with a raw key.
    fn decrypt_v1_blob(key_bytes: [u8; 32], blob: &str) -> Option<String> {
        let parts: Vec<&str> = blob.split(':').collect();
        if parts.len() != 3 {
            return None;
        }
        let iv = hex::decode(parts[0]).ok()?;
        let tag = hex::decode(parts[1]).ok()?;
        let ct = hex::decode(parts[2]).ok()?;
        let key = Key::<Aes256Gcm>::from_slice(&key_bytes);
        let cipher = Aes256Gcm::new(key);
        let mut full = ct;
        full.extend_from_slice(&tag);
        let nonce = Nonce::from_slice(&iv);
        let plain = cipher.decrypt(nonce, full.as_ref()).ok()?;
        String::from_utf8(plain).ok()
    }

    /// Decrypt a legacy Rust v1 slot (SHA256-stretched KDF, per-slot salt).
    fn decrypt_legacy_sha256(secret: &str, blob: &str, salt: &[u8]) -> Option<String> {
        Self::decrypt_v1_blob(Self::derive_key_v1_rust(secret, salt), blob)
    }

    /// Decrypt a legacy TS v1 slot (plain scrypt(passphrase, salt, 32)).
    fn decrypt_legacy_scrypt(secret: &str, blob: &str, salt_hex: &str) -> Option<String> {
        let salt = hex::decode(salt_hex).ok()?;
        let key = Self::scrypt_32(secret.as_bytes(), &salt).ok()?;
        Self::decrypt_v1_blob(key, blob)
    }

    /// Read the slot value from the loaded vault ONLY (no env fallback).
    /// Understands the unified v2 format and both legacy v1 layouts.
    fn read_slot(any: &VaultFileAny, secret: &str, slot_id: &str) -> Option<String> {
        match any {
            VaultFileAny::V2(v) => {
                let blob = v.slots.get(slot_id)?;
                Self::decrypt_v2(secret, blob, &v.salt).ok()
            }
            VaultFileAny::V1Rust(v1) => {
                let blob = v1.data.get(slot_id)?;
                let salt_hex = v1.salts.get(slot_id)?;
                let salt = hex::decode(salt_hex).ok()?;
                Self::decrypt_legacy_sha256(secret, blob, &salt)
            }
            VaultFileAny::V1Ts(v1) => {
                let blob = v1.data.get(slot_id)?;
                Self::decrypt_legacy_scrypt(secret, blob, &v1.salt)
            }
        }
    }

    /// Store a key for a slot. Empty value deletes the entry. The first
    /// write converts any legacy v1 file (Rust or TS layout) into the
    /// unified v2 format, re-encrypting all previously stored keys.
    pub async fn set_api_key(&self, slot_id: &str, value: &str) -> Result<(), String> {
        if find_slot(slot_id).is_none() {
            return Err("unknown slot".to_string());
        }
        let _guard = self.io_lock.lock().await;
        let secret = self.load_secret()?;
        let mut v = self.load_or_migrate(&secret);
        if value.trim().is_empty() {
            v.slots.remove(slot_id);
        } else {
            let blob = Self::encrypt_v2(&secret, value.trim(), &v.salt)?;
            v.slots.insert(slot_id.to_string(), blob);
        }
        self.save(&v)
    }

    /// Read the effective key for a slot: vault wins, then env var fallback.
    /// This is the ONLY key-reading entry point — never log its result.
    pub async fn get_api_key(&self, slot_id: &str) -> Option<String> {
        {
            let _guard = self.io_lock.lock().await;
            if let Ok(secret) = self.load_secret() {
                let any = self.load();
                if let Some(plain) = Self::read_slot(&any, &secret, slot_id) {
                    return Some(plain);
                }
            }
        }
        let slot = find_slot(slot_id)?;
        std::env::var(slot.env_var).ok().filter(|s| !s.trim().is_empty()).map(|s| s.trim().to_string())
    }

    /// Masked, panel-safe status for every slot.
    pub async fn statuses(&self) -> Vec<ApiKeySlotStatus> {
        let mut out = Vec::with_capacity(API_KEY_SLOTS.len());
        for slot in API_KEY_SLOTS {
            let from_vault = {
                let _guard = self.io_lock.lock().await;
                self.load_secret().ok().and_then(|secret| {
                    let any = self.load();
                    Self::read_slot(&any, &secret, slot.id)
                })
            };
            let env_val = std::env::var(slot.env_var).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
            let value = from_vault.clone().or_else(|| env_val.clone());
            out.push(ApiKeySlotStatus {
                id: slot.id.to_string(),
                label: slot.label.to_string(),
                purpose: slot.purpose.to_string(),
                signup_url: slot.signup_url.to_string(),
                pricing: slot.pricing.as_str().to_string(),
                env_var: slot.env_var.to_string(),
                placeholder: slot.placeholder.to_string(),
                network: slot.network.as_str().to_string(),
                configured: value.is_some(),
                masked: value.as_deref().map(mask_secret),
                source: if from_vault.is_some() { Some("vault") } else if env_val.is_some() { Some("env") } else { None },
            });
        }
        out
    }

    /// Health info about the vault file (permissions warning).
    pub fn vault_health(&self) -> serde_json::Value {
        let path = self.vault_path();
        if !path.exists() {
            return serde_json::json!({ "file": path.display().to_string(), "exists": false });
        }
        let mode_ok = file_permissions_tight(&path);
        serde_json::json!({
            "file": path.display().to_string(),
            "exists": true,
            "permissions_ok": mode_ok,
            "warning": if mode_ok { None } else { Some("vault file is group/world-readable — chmod 600 recommended") },
        })
    }
}

/// Mask a secret for display: keep at most the first 2 and last 4 chars.
pub fn mask_secret(value: &str) -> String {
    let v = value.trim();
    if v.chars().count() <= 8 {
        return "••••".to_string();
    }
    let head: String = v.chars().take(2).collect();
    let tail: String = v.chars().rev().take(4).collect::<String>().chars().rev().collect();
    format!("{}…{}", head, tail)
}

fn random_bytes(n: usize) -> Vec<u8> {
    use rand::RngCore;
    let mut buf = vec![0u8; n];
    rand::thread_rng().fill_bytes(&mut buf);
    buf
}

fn random_hex(n: usize) -> String {
    hex::encode(random_bytes(n))
}

#[cfg(unix)]
fn restrict_permissions(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &std::path::Path) {}

#[cfg(unix)]
fn file_permissions_tight(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(m) => m.permissions().mode() & 0o077 == 0,
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn file_permissions_tight(_path: &std::path::Path) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault(tag: &str) -> (ApiKeyVault, PathBuf) {
        let dir = std::env::temp_dir().join(format!("girasol-vault-test-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        (ApiKeyVault::new(dir.clone()), dir)
    }

    /// Serialize tests that mutate VAULT_PASSPHRASE — parallel tokio::test
    /// threads share the process environment, and without this the rotation
    /// test races every other vault test.
    static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[tokio::test]
    async fn test_set_get_mask_roundtrip() {
        let _env = ENV_LOCK.lock().await;
        std::env::remove_var("VAULT_PASSPHRASE");
        let (vault, _dir) = temp_vault("rt");
        vault.set_api_key("helius", "abcdef1234567890xyz").await.unwrap();
        let got = vault.get_api_key("helius").await.unwrap();
        assert_eq!(got, "abcdef1234567890xyz");
        // masked never contains full key
        let statuses = vault.statuses().await;
        let helius = statuses.iter().find(|s| s.id == "helius").unwrap();
        assert!(helius.configured);
        assert_eq!(helius.source, Some("vault"));
        assert_eq!(helius.network, "both");
        let masked = helius.masked.clone().unwrap();
        assert!(!masked.contains("abcdef1234567890xyz"));
        assert!(masked.contains("…"));
    }

    #[tokio::test]
    async fn test_empty_value_deletes() {
        let _env = ENV_LOCK.lock().await;
        std::env::remove_var("VAULT_PASSPHRASE");
        let (vault, _dir) = temp_vault("del");
        vault.set_api_key("jito", "some-key").await.unwrap();
        vault.set_api_key("jito", "").await.unwrap();
        assert!(vault.get_api_key("jito").await.is_none());
        let statuses = vault.statuses().await;
        let jito = statuses.iter().find(|s| s.id == "jito").unwrap();
        assert!(!jito.configured);
        assert_eq!(jito.source, None);
    }

    #[tokio::test]
    async fn test_unknown_slot_rejected() {
        let (vault, _dir) = temp_vault("unknown");
        assert!(vault.set_api_key("not-a-slot", "x").await.is_err());
    }

    #[tokio::test]
    async fn test_passphrase_rotation_breaks_decryption() {
        let _env = ENV_LOCK.lock().await;
        std::env::set_var("VAULT_PASSPHRASE", "pass-one");
        let (vault, _dir) = temp_vault("rotate");
        vault.set_api_key("helius", "secret-value").await.unwrap();
        std::env::set_var("VAULT_PASSPHRASE", "pass-two");
        // Decryption with the wrong passphrase fails cleanly (no plaintext).
        assert!(vault.get_api_key("helius").await.is_none());
        std::env::remove_var("VAULT_PASSPHRASE");
    }

    #[tokio::test]
    async fn test_vault_file_encrypted_at_rest() {
        let _env = ENV_LOCK.lock().await;
        std::env::remove_var("VAULT_PASSPHRASE");
        let (vault, dir) = temp_vault("atrest");
        vault.set_api_key("nozomi", "super-secret-token-value").await.unwrap();
        let raw = std::fs::read_to_string(vault.vault_path()).unwrap();
        assert!(!raw.contains("super-secret-token-value"), "plaintext leaked to vault file");
        // v2 format on disk: version 2, kdf scrypt, slots {iv, ct, tag}.
        let parsed: VaultFileV2 = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed.version, VAULT_VERSION_V2);
        assert_eq!(parsed.kdf, "scrypt");
        assert_eq!(parsed.kdf_params, ScryptParamsV2::default());
        assert!(hex::decode(&parsed.salt).map(|s| s.len() == 32).unwrap_or(false));
        assert!(parsed.slots.contains_key("nozomi"));
        let _ = dir;
    }

    #[test]
    fn test_mask_secret_short_values() {
        assert_eq!(mask_secret("short"), "••••");
        assert_eq!(mask_secret(""), "••••");
        let m = mask_secret("sk-ant-very-long-key-1234");
        assert!(m.starts_with("sk"));
        assert!(m.ends_with("1234"));
        assert!(m.contains('…'));
    }

    #[test]
    fn test_slots_catalog_has_required_providers() {
        let ids: Vec<&str> = API_KEY_SLOTS.iter().map(|s| s.id).collect();
        for required in ["rpc-http", "rpc-wss", "erpc", "geyser", "shreds", "jito", "bloxroute", "nextblock", "zeroslot", "nozomi", "helius"] {
            assert!(ids.contains(&required), "missing slot: {}", required);
        }
        // Unified catalog: EVM slots shared from the TS bot.
        for required in ["zerox", "dexscreener", "coingecko", "alchemy", "quicknode", "infura", "explorer"] {
            assert!(ids.contains(&required), "missing EVM slot: {}", required);
        }
        // every slot has a unique id + non-empty signup/placeholder
        let mut seen = std::collections::HashSet::new();
        for s in API_KEY_SLOTS {
            assert!(seen.insert(s.id), "duplicate slot id: {}", s.id);
            assert!(!s.signup_url.is_empty());
            assert!(!s.placeholder.is_empty());
        }
        // network field present and correct
        for expected in ["rpc-http", "rpc-wss", "erpc", "geyser", "shreds", "jito", "bloxroute", "nextblock", "zeroslot", "nozomi"] {
            assert_eq!(find_slot(expected).unwrap().network.as_str(), "solana", "slot {}", expected);
        }
        for expected in ["zerox", "dexscreener", "coingecko", "alchemy", "quicknode", "infura", "explorer"] {
            assert_eq!(find_slot(expected).unwrap().network.as_str(), "evm", "slot {}", expected);
        }
        // helius is shared — a single slot, not duplicated per network
        assert_eq!(API_KEY_SLOTS.iter().filter(|s| s.id == "helius").count(), 1);
        assert_eq!(find_slot("helius").unwrap().network.as_str(), "both");
    }

    #[tokio::test]
    async fn test_encrypt_decrypt_roundtrip_direct() {
        let salt = hex::encode(vec![1u8; 32]);
        let blob = ApiKeyVault::encrypt_v2("pass", "hello-world", &salt).unwrap();
        assert_eq!(ApiKeyVault::decrypt_v2("pass", &blob, &salt).unwrap(), "hello-world");
        assert!(ApiKeyVault::decrypt_v2("wrong", &blob, &salt).is_err());
    }

    // ---- Unified v2 cross-bot round-trip tests ----

    /// Shared cross-bot test vector (fixed by the mission):
    /// passphrase "roundtrip-test-1234", slot "alchemy", value "sk-test-vector-00".
    const RT_PASSPHRASE: &str = "roundtrip-test-1234";
    const RT_SLOT: &str = "alchemy";
    const RT_VALUE: &str = "sk-test-vector-00";

    /// Build a v2 vault file exactly the way the TS bot writes it
    /// (node:crypto scryptSync(pass, salt, 32, {N:16384,r:8,p:1}) →
    /// AES-256-GCM) — synthetic, no TS involved at runtime. The TS side has
    /// the mirror test (Rust-vault emulation) in tests/api-key-vault.test.ts.
    fn write_ts_style_v2_file(dir: &std::path::Path, value: &str) {
        let salt = random_bytes(32);
        let key = ApiKeyVault::scrypt_32(RT_PASSPHRASE.as_bytes(), &salt).unwrap();
        let key_gcm = Key::<Aes256Gcm>::from_slice(&key);
        let cipher = Aes256Gcm::new(key_gcm);
        let iv = random_bytes(12);
        let ct = cipher
            .encrypt(Nonce::from_slice(&iv), value.as_bytes())
            .unwrap();
        let (body, tag) = ct.split_at(ct.len() - 16);
        let mut slots = HashMap::new();
        slots.insert(
            RT_SLOT.to_string(),
            SlotBlobV2 {
                iv: hex::encode(&iv),
                ct: hex::encode(body),
                tag: hex::encode(tag),
            },
        );
        let file = VaultFileV2 {
            version: VAULT_VERSION_V2,
            kdf: "scrypt".to_string(),
            kdf_params: ScryptParamsV2::default(),
            salt: hex::encode(&salt),
            slots,
        };
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("apikeys.enc.json"), serde_json::to_string_pretty(&file).unwrap()).unwrap();
    }

    #[tokio::test]
    async fn test_roundtrip_ts_written_v2_file() {
        let _env = ENV_LOCK.lock().await;
        std::env::set_var("VAULT_PASSPHRASE", RT_PASSPHRASE);
        let (vault, dir) = temp_vault("rt-ts");
        write_ts_style_v2_file(&dir, RT_VALUE);
        let got = vault.get_api_key(RT_SLOT).await.unwrap();
        assert_eq!(got, RT_VALUE);
        std::env::remove_var("VAULT_PASSPHRASE");
    }

    #[tokio::test]
    async fn test_v1_rust_file_migrates_to_v2_on_first_write() {
        let _env = ENV_LOCK.lock().await;
        std::env::set_var("VAULT_PASSPHRASE", "test-passphrase-123");
        let (vault, dir) = temp_vault("migr1");
        // Legacy Rust v1 file: per-slot salt, SHA256-stretched KDF.
        let salt = random_bytes(16);
        let key = ApiKeyVault::derive_key_v1_rust("test-passphrase-123", &salt);
        let key_gcm = Key::<Aes256Gcm>::from_slice(&key);
        let cipher = Aes256Gcm::new(key_gcm);
        let iv = random_bytes(12);
        let ct = cipher.encrypt(Nonce::from_slice(&iv), b"legacy-solana-key" as &[u8]).unwrap();
        let (body, tag) = ct.split_at(ct.len() - 16);
        let mut salts = HashMap::new();
        salts.insert("jito".to_string(), hex::encode(&salt));
        let mut data = HashMap::new();
        data.insert(
            "jito".to_string(),
            format!("{}:{}:{}", hex::encode(&iv), hex::encode(tag), hex::encode(body)),
        );
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("apikeys.enc.json"),
            serde_json::to_string_pretty(&VaultFileV1Rust { version: VAULT_VERSION_V1, salts, data }).unwrap(),
        ).unwrap();
        // Readable BEFORE migration (v1 read path).
        assert_eq!(vault.get_api_key("jito").await.unwrap(), "legacy-solana-key");
        // First write converts the whole file to v2 and preserves the old key.
        vault.set_api_key("nozomi", "fresh-key").await.unwrap();
        assert_eq!(vault.get_api_key("jito").await.unwrap(), "legacy-solana-key");
        assert_eq!(vault.get_api_key("nozomi").await.unwrap(), "fresh-key");
        let raw = std::fs::read_to_string(dir.join("apikeys.enc.json")).unwrap();
        let parsed: VaultFileV2 = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed.version, VAULT_VERSION_V2);
        assert!(parsed.slots.contains_key("jito"));
        std::env::remove_var("VAULT_PASSPHRASE");
    }

    #[tokio::test]
    async fn test_v1_ts_file_migrates_to_v2_on_first_write() {
        let _env = ENV_LOCK.lock().await;
        std::env::set_var("VAULT_PASSPHRASE", "test-passphrase-123");
        let (vault, dir) = temp_vault("migr2");
        // Legacy TS v1 layout: single salt, plain scrypt KDF.
        let salt = random_bytes(16);
        let key = ApiKeyVault::scrypt_32("test-passphrase-123".as_bytes(), &salt).unwrap();
        let key_gcm = Key::<Aes256Gcm>::from_slice(&key);
        let cipher = Aes256Gcm::new(key_gcm);
        let iv = random_bytes(12);
        let ct = cipher.encrypt(Nonce::from_slice(&iv), b"legacy-evm-key" as &[u8]).unwrap();
        let (body, tag) = ct.split_at(ct.len() - 16);
        let mut data = HashMap::new();
        data.insert(
            "zerox".to_string(),
            format!("{}:{}:{}", hex::encode(&iv), hex::encode(tag), hex::encode(body)),
        );
        #[derive(Serialize)]
        struct V1TsFile<'a> {
            version: u32,
            salt: String,
            data: &'a HashMap<String, String>,
        }
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("apikeys.enc.json"),
            serde_json::to_string_pretty(&V1TsFile { version: VAULT_VERSION_V1, salt: hex::encode(&salt), data: &data }).unwrap(),
        ).unwrap();
        assert_eq!(vault.get_api_key("zerox").await.unwrap(), "legacy-evm-key");
        vault.set_api_key("helius", "fresh-helius").await.unwrap();
        assert_eq!(vault.get_api_key("zerox").await.unwrap(), "legacy-evm-key");
        assert_eq!(vault.get_api_key("helius").await.unwrap(), "fresh-helius");
        let raw = std::fs::read_to_string(dir.join("apikeys.enc.json")).unwrap();
        let parsed: VaultFileV2 = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed.version, VAULT_VERSION_V2);
        assert!(parsed.slots.contains_key("zerox"));
        std::env::remove_var("VAULT_PASSPHRASE");
    }
}