//! Bounded RLP envelopes for Nitro signed user transactions.
use anyhow::Result;
#[derive(Clone, Debug)]
pub struct ObservedTx {
    pub hash: String,
    pub from: String,
    pub nonce: u64,
    pub to: Option<String>,
    pub value: u128,
    pub input: Vec<u8>,
    pub chain_id: u64,
}
const MAX_PAYLOAD: usize = 256 * 1024;

#[derive(Clone, Copy)]
struct Rlp<'a> {
    payload: &'a [u8],
    list: bool,
}
fn item<'a>(input: &mut &'a [u8]) -> Result<Rlp<'a>> {
    anyhow::ensure!(!input.is_empty(), "truncated RLP");
    let first = input[0];
    if first < 0x80 {
        let payload = &input[..1];
        *input = &input[1..];
        return Ok(Rlp {
            payload,
            list: false,
        });
    }
    let (list, offset) = if first >= 0xc0 {
        (true, 0xc0)
    } else {
        (false, 0x80)
    };
    let prefix = (first - offset) as usize;
    let (header, len) = if prefix <= 55 {
        (1, prefix)
    } else {
        let n = prefix - 55;
        anyhow::ensure!(
            n <= 8 && input.len() > n && input[1] != 0,
            "invalid RLP length"
        );
        let mut length = 0usize;
        for byte in &input[1..=n] {
            length = length
                .checked_mul(256)
                .and_then(|v| v.checked_add(*byte as usize))
                .ok_or_else(|| anyhow::anyhow!("RLP overflow"))?;
        }
        anyhow::ensure!(length >= 56, "noncanonical RLP length");
        (1 + n, length)
    };
    anyhow::ensure!(
        len <= MAX_PAYLOAD && header + len <= input.len(),
        "truncated or oversized RLP"
    );
    let payload = &input[header..header + len];
    anyhow::ensure!(
        list || len != 1 || payload[0] >= 0x80,
        "noncanonical RLP string"
    );
    *input = &input[header + len..];
    Ok(Rlp { payload, list })
}
fn fields(payload: &[u8]) -> Result<Vec<Rlp<'_>>> {
    let mut remaining = payload;
    let mut values = Vec::new();
    while !remaining.is_empty() {
        anyhow::ensure!(values.len() < 16, "too many transaction fields");
        values.push(item(&mut remaining)?);
    }
    Ok(values)
}
fn number(value: Rlp<'_>) -> Result<u128> {
    anyhow::ensure!(
        !value.list && value.payload.len() <= 16 && value.payload.first() != Some(&0),
        "invalid integer or >128-bit quantity"
    );
    Ok(value.payload.iter().fold(0, |n, b| (n << 8) | *b as u128))
}
fn encode(payload: &[u8], list: bool) -> Vec<u8> {
    if !list && payload.len() == 1 && payload[0] < 0x80 {
        return payload.to_vec();
    }
    let base = if list { 0xc0 } else { 0x80 };
    let mut out = Vec::new();
    if payload.len() < 56 {
        out.push(base + payload.len() as u8);
    } else {
        let len = payload.len().to_be_bytes();
        let first = len.iter().position(|x| *x != 0).unwrap();
        out.push(base + 55 + (len.len() - first) as u8);
        out.extend_from_slice(&len[first..]);
    }
    out.extend_from_slice(payload);
    out
}
fn integer(value: u128) -> Vec<u8> {
    let bytes = value.to_be_bytes();
    encode(
        &bytes[bytes.iter().position(|x| *x != 0).unwrap_or(16)..],
        false,
    )
}
fn digest(bytes: &[u8]) -> [u8; 32] {
    {
        use sha3::Digest;
        sha3::Keccak256::digest(bytes).into()
    }
}
fn address(public: &libsecp256k1::PublicKey) -> String {
    format!("0x{}", hex::encode(&digest(&public.serialize()[1..])[12..]))
}

/// Only standard replayable user envelopes: legacy (EIP-155), EIP-2930, EIP-1559.
/// Blob/delegation/Arbitrum system transactions cannot be reauthorized this way.
pub fn decode_signed(raw: &[u8]) -> Result<ObservedTx> {
    anyhow::ensure!(
        !raw.is_empty() && raw.len() <= MAX_PAYLOAD,
        "invalid envelope size"
    );
    let typed = raw[0] < 0x80;
    anyhow::ensure!(
        !typed || matches!(raw[0], 1 | 2),
        "unsupported envelope type"
    );
    let mut cursor = if typed { &raw[1..] } else { raw };
    let root = item(&mut cursor)?;
    anyhow::ensure!(root.list && cursor.is_empty(), "invalid envelope root");
    let f = fields(root.payload)?;
    let (unsigned_count, to_index, value_index, input_index, chain, parity) = if typed {
        let (n, to) = if raw[0] == 2 { (9, 5) } else { (8, 4) };
        anyhow::ensure!(f.len() == n + 3, "invalid typed field count");
        anyhow::ensure!(f[n - 1].list, "missing access list");
        (
            n,
            to,
            to + 1,
            to + 2,
            u64::try_from(number(f[0])?)?,
            u8::try_from(number(f[n])?)?,
        )
    } else {
        anyhow::ensure!(f.len() == 9, "invalid legacy field count");
        let v = number(f[6])?;
        anyhow::ensure!(v >= 35, "unprotected legacy transaction");
        (
            6,
            3,
            4,
            5,
            u64::try_from((v - 35) / 2)?,
            ((v - 35) % 2) as u8,
        )
    };
    anyhow::ensure!(chain > 0 && parity <= 1, "invalid chain or parity");
    let to = f[to_index];
    anyhow::ensure!(
        !to.list && matches!(to.payload.len(), 0 | 20),
        "invalid destination"
    );
    anyhow::ensure!(!f[input_index].list, "invalid calldata");
    let mut unsigned = Vec::new();
    for value in &f[..unsigned_count] {
        unsigned.extend(encode(value.payload, value.list));
    }
    if !typed {
        unsigned.extend(integer(chain as u128));
        unsigned.extend([0x80, 0x80]);
    }
    let mut signing = encode(&unsigned, true);
    if typed {
        signing.insert(0, raw[0]);
    }
    let mut signature = [0u8; 64];
    for (idx, field) in f[unsigned_count + 1..].iter().enumerate() {
        anyhow::ensure!(
            !field.list
                && !field.payload.is_empty()
                && field.payload.len() <= 32
                && field.payload[0] != 0,
            "invalid signature scalar"
        );
        signature[idx * 32 + 32 - field.payload.len()..idx * 32 + 32]
            .copy_from_slice(field.payload);
    }
    let signature = libsecp256k1::Signature::parse_standard(&signature)?;
    let message = libsecp256k1::Message::parse(&digest(&signing));
    let public = libsecp256k1::recover(
        &message,
        &signature,
        &libsecp256k1::RecoveryId::parse(parity)?,
    )?;
    Ok(ObservedTx {
        hash: format!("0x{}", hex::encode(digest(raw))),
        from: address(&public),
        nonce: u64::try_from(number(f[if typed { 1 } else { 0 }])?)?,
        to: if to.payload.is_empty() {
            None
        } else {
            Some(format!("0x{}", hex::encode(to.payload)))
        },
        value: number(f[value_index])?,
        input: f[input_index].payload.to_vec(),
        chain_id: chain,
    })
}

/// Fresh EIP-1559 envelope: own nonce/signer, original destination/value/calldata.
pub fn sign_replay(
    chain: u64,
    nonce: u64,
    gas: u64,
    fee: u128,
    to: Option<&str>,
    value: u128,
    input: &[u8],
    key: &str,
) -> Result<(Vec<u8>, String)> {
    anyhow::ensure!(
        chain > 0 && gas >= 21000 && input.len() <= MAX_PAYLOAD,
        "invalid replay parameters"
    );
    let key = hex::decode(key.strip_prefix("0x").unwrap_or(key))?;
    let secret = libsecp256k1::SecretKey::parse_slice(&key)?;
    let public = libsecp256k1::PublicKey::from_secret_key(&secret);
    let destination = match to {
        Some(a) => {
            anyhow::ensure!(a.starts_with("0x") && a.len() == 42, "invalid destination");
            hex::decode(&a[2..])?
        }
        None => Vec::new(),
    };
    let mut unsigned = Vec::new();
    for n in [chain as u128, nonce as u128, 0, fee, gas as u128] {
        unsigned.extend(integer(n));
    }
    unsigned.extend(encode(&destination, false));
    unsigned.extend(integer(value));
    unsigned.extend(encode(input, false));
    unsigned.push(0xc0);
    let signing = [&[2][..], &encode(&unsigned, true)].concat();
    let (signature, recovery) =
        libsecp256k1::sign(&libsecp256k1::Message::parse(&digest(&signing)), &secret);
    anyhow::ensure!(recovery.serialize() <= 1, "unsupported recovery parity");
    let signature = signature.serialize();
    unsigned.extend(integer(recovery.serialize() as u128));
    for scalar in signature.chunks(32) {
        let start = scalar.iter().position(|b| *b != 0).unwrap_or(32);
        unsigned.extend(encode(&scalar[start..], false));
    }
    Ok((
        [&[2][..], &encode(&unsigned, true)].concat(),
        address(&public),
    ))
}

/// Nitro framing ported from arbitrum-alloy reader.rs: signed=4, batch=3,
/// heartbeat=6. L1/system/delegated envelopes are deliberately unsupported.
pub fn decode_l2(payload: &[u8], chain: u64) -> Result<Vec<ObservedTx>> {
    fn decode(payload: &[u8], chain: u64, depth: u8) -> Result<Vec<ObservedTx>> {
        anyhow::ensure!(
            depth <= 16 && !payload.is_empty() && payload.len() <= MAX_PAYLOAD,
            "invalid Nitro payload"
        );
        match payload[0] {
            4 => {
                let tx = decode_signed(&payload[1..])?;
                anyhow::ensure!(tx.chain_id == chain, "feed transaction chain mismatch");
                Ok(vec![tx])
            }
            6 => Ok(Vec::new()),
            3 => {
                let mut rest = &payload[1..];
                let mut txs = Vec::new();
                while !rest.is_empty() {
                    anyhow::ensure!(rest.len() >= 8, "truncated batch length");
                    let len = usize::try_from(u64::from_be_bytes(rest[..8].try_into()?))?;
                    rest = &rest[8..];
                    anyhow::ensure!(len > 0 && len <= rest.len(), "truncated batch segment");
                    txs.extend(decode(&rest[..len], chain, depth + 1)?);
                    rest = &rest[len..];
                }
                Ok(txs)
            }
            kind => anyhow::bail!("unsupported Nitro message kind {kind}"),
        }
    }
    decode(payload, chain, 0)
}
#[cfg(test)]
mod tests {
    use super::*;
    const KEY: &str = "0000000000000000000000000000000000000000000000000000000000000001";
    #[test]
    fn signs_and_recovers_full_calldata_and_native_value() {
        let to = "0x1111111111111111111111111111111111111111";
        let (raw, signer) = sign_replay(
            99801,
            9,
            123456,
            700,
            Some(to),
            91,
            &[0xde, 0xad, 0xbe, 0xef],
            KEY,
        )
        .unwrap();
        assert_eq!(signer, "0x7e5f4552091a69125d5dfcb7b8c2659029395bdf");
        let tx = decode_signed(&raw).unwrap();
        assert_eq!(tx.from, signer);
        assert_eq!(tx.to.as_deref(), Some(to));
        assert_eq!(tx.chain_id, 99801);
        assert_eq!(tx.nonce, 9);
        assert_eq!(tx.value, 91);
        assert_eq!(tx.input, vec![0xde, 0xad, 0xbe, 0xef]);
    }
    #[test]
    fn nitro_batch_preserves_all_transactions_and_rejects_wrong_chain() {
        let (raw, _) = sign_replay(46630, 0, 21000, 1, None, 0, &[], KEY).unwrap();
        let signed = [&[4][..], &raw].concat();
        let batch = [
            &[3][..],
            &(signed.len() as u64).to_be_bytes(),
            &signed,
            &1u64.to_be_bytes(),
            &[6],
        ]
        .concat();
        assert_eq!(decode_l2(&batch, 46630).unwrap().len(), 1);
        assert!(decode_l2(&batch, 4663).is_err());
    }
    #[test]
    fn malformed_and_unsupported_envelopes_fail_closed() {
        for payload in [&[][..], &[4], &[4, 3], &[4, 4], &[3, 0, 0], &[0]] {
            assert!(decode_l2(payload, 46630).is_err());
        }
    }
}
