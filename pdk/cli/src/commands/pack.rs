use anyhow::{bail, Context, Result};
use clap::Parser;
use colored::Colorize;
use serde::Deserialize;
use std::{
    fs::{self, File},
    io::Write,
    path::PathBuf,
};

#[derive(Parser, Debug)]
pub struct PackArgs {
    /// Output path for the .prxplugin file (default: <plugin-name>.prxplugin)
    #[arg(long, short)]
    pub output: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
struct PluginMeta {
    plugin: PluginMetaInner,
}

#[derive(Debug, Deserialize)]
struct PluginMetaInner {
    name: String,
    version: String,
}

pub fn run(args: PackArgs) -> Result<()> {
    let dir = std::env::current_dir().context("Failed to get current directory")?;

    // ── Require plugin.wasm ───────────────────────────────────────────────
    let wasm_path = dir.join("plugin.wasm");
    if !wasm_path.exists() {
        bail!(
            "plugin.wasm not found. Run 'prx-plugin build' first."
        );
    }

    // ── Read plugin.toml for metadata ────────────────────────────────────
    let toml_path = dir.join("plugin.toml");
    if !toml_path.exists() {
        bail!("plugin.toml not found in current directory.");
    }

    let toml_str = fs::read_to_string(&toml_path)
        .context("Cannot read plugin.toml")?;
    let meta: PluginMeta = toml::from_str(&toml_str)
        .context("Failed to parse plugin.toml")?;

    let plugin_name = &meta.plugin.name;
    let plugin_version = &meta.plugin.version;

    // ── Determine output filename ─────────────────────────────────────────
    let out_path = args.output.unwrap_or_else(|| {
        dir.join(format!("{}-{}.prxplugin", plugin_name, plugin_version))
    });

    println!(
        "{} {} v{} → {}",
        "Packing".green().bold(),
        plugin_name.cyan(),
        plugin_version,
        out_path.display().to_string().yellow()
    );

    // ── Build file list ───────────────────────────────────────────────────
    let mut files: Vec<(PathBuf, String)> = vec![
        (wasm_path, "plugin.wasm".to_string()),
        (toml_path, "plugin.toml".to_string()),
    ];

    // Optional files
    for opt in &["README.md", "LICENSE", "CHANGELOG.md"] {
        let p = dir.join(opt);
        if p.exists() {
            files.push((p, opt.to_string()));
        }
    }

    // ── Create tar.gz (manual implementation, no external crate) ─────────
    let out_file = File::create(&out_path)
        .with_context(|| format!("Cannot create {}", out_path.display()))?;

    let tar_bytes = build_tar(&files, plugin_name, plugin_version)?;
    let gz_bytes = gzip_compress(&tar_bytes)?;

    (&out_file).write_all(&gz_bytes)
        .context("Failed to write .prxplugin archive")?;

    let size_kb = gz_bytes.len() as f64 / 1024.0;
    println!("  {} {} ({:.1} KB)", "Created".green(), out_path.display(), size_kb);
    println!("  {} Contents:", "ℹ".blue());
    for (src, name) in &files {
        let fsize = fs::metadata(src).map(|m| m.len()).unwrap_or(0);
        println!("    {} {} ({} bytes)", "·".dimmed(), name, fsize);
    }
    // Also mention the checksums file we embed
    println!("    {} checksums.sha256 (generated)", "·".dimmed());

    println!("{}", "Pack complete.".green().bold());
    Ok(())
}

// ── Minimal tar builder ────────────────────────────────────────────────────

fn build_tar(files: &[(PathBuf, String)], plugin_name: &str, version: &str) -> Result<Vec<u8>> {
    let mut tar: Vec<u8> = Vec::new();
    let prefix = format!("{}-{}", plugin_name, version);

    // Compute checksums content first
    let mut checksum_lines = String::new();
    for (src, name) in files {
        let data = fs::read(src)?;
        let hash = sha256_hex(&data);
        checksum_lines.push_str(&format!("{}  {}\n", hash, name));
    }

    // Write regular files
    for (src, name) in files {
        let data = fs::read(src)?;
        let entry_name = format!("{}/{}", prefix, name);
        write_tar_entry(&mut tar, &entry_name, &data)?;
    }

    // Write checksums
    let checksum_name = format!("{}/checksums.sha256", prefix);
    write_tar_entry(&mut tar, &checksum_name, checksum_lines.as_bytes())?;

    // End-of-archive: two 512-byte zero blocks
    tar.extend_from_slice(&[0u8; 1024]);
    Ok(tar)
}

fn write_tar_entry(tar: &mut Vec<u8>, name: &str, data: &[u8]) -> Result<()> {
    let mut header = [0u8; 512];
    let name_bytes = name.as_bytes();
    let name_len = name_bytes.len().min(99);
    header[..name_len].copy_from_slice(&name_bytes[..name_len]);

    // File mode: 0644
    let mode = b"0000644\0";
    header[100..108].copy_from_slice(mode);
    // uid / gid
    header[108..116].copy_from_slice(b"0000000\0");
    header[116..124].copy_from_slice(b"0000000\0");
    // size (octal, 11 digits + space)
    let size_oct = format!("{:011o} ", data.len());
    header[124..136].copy_from_slice(size_oct.as_bytes());
    // mtime
    header[136..148].copy_from_slice(b"00000000000 ");
    // typeflag: regular file
    header[156] = b'0';
    // magic (ustar)
    header[257..263].copy_from_slice(b"ustar ");
    header[263..265].copy_from_slice(b" \0");

    // Checksum placeholder: spaces
    header[148..156].copy_from_slice(b"        ");
    let checksum: u32 = header.iter().map(|&b| b as u32).sum();
    let cksum = format!("{:06o}\0 ", checksum);
    header[148..156].copy_from_slice(cksum.as_bytes());

    tar.extend_from_slice(&header);
    tar.extend_from_slice(data);
    // Pad to 512-byte boundary
    let remainder = (512 - data.len() % 512) % 512;
    tar.extend_from_slice(&vec![0u8; remainder]);
    Ok(())
}

/// Very simple SHA-256 implementation (no external crate).
/// Uses a pure-Rust computation.
fn sha256_hex(data: &[u8]) -> String {
    // We use the standard sha2 approach via bitwise ops.
    // Since we don't have sha2 crate, we implement it manually.
    let hash = sha256(data);
    hash.iter().map(|b| format!("{:02x}", b)).collect()
}

// Pure-Rust SHA-256 (FIPS 180-4)
fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a,
        0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    ];
    let k: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5,
        0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
        0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3,
        0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
        0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc,
        0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
        0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
        0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
        0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3,
        0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5,
        0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208,
        0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];

    let bit_len = (data.len() as u64) * 8;
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([chunk[i*4], chunk[i*4+1], chunk[i*4+2], chunk[i*4+3]]);
        }
        for i in 16..64 {
            let s0 = w[i-15].rotate_right(7) ^ w[i-15].rotate_right(18) ^ (w[i-15] >> 3);
            let s1 = w[i-2].rotate_right(17) ^ w[i-2].rotate_right(19) ^ (w[i-2] >> 10);
            w[i] = w[i-16].wrapping_add(s0).wrapping_add(w[i-7]).wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] =
            [h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]];
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh.wrapping_add(s1).wrapping_add(ch).wrapping_add(k[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            hh = g; g = f; f = e;
            e = d.wrapping_add(temp1);
            d = c; c = b; b = a;
            a = temp1.wrapping_add(temp2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut out = [0u8; 32];
    for (i, &word) in h.iter().enumerate() {
        out[i*4..(i+1)*4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

// ── Minimal gzip compressor ────────────────────────────────────────────────
// We use a "store" (no compression) gzip format for simplicity,
// since we have no flate2 crate available.

fn gzip_compress(data: &[u8]) -> Result<Vec<u8>> {
    // gzip header: ID1 ID2 CM FLG MTIME XFL OS
    let mut out = Vec::new();
    out.push(0x1f); // ID1
    out.push(0x8b); // ID2
    out.push(0x08); // CM = deflate
    out.push(0x00); // FLG = none
    out.extend_from_slice(&[0x00; 4]); // MTIME = 0
    out.push(0x00); // XFL
    out.push(0xff); // OS = unknown

    // Deflate: stored blocks
    let deflated = deflate_store(data);
    out.extend_from_slice(&deflated);

    // CRC32 and size
    let crc = crc32(data);
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    Ok(out)
}

/// Deflate "store" (BTYPE=00, no compression).
fn deflate_store(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut remaining = data;
    while !remaining.is_empty() {
        let chunk_size = remaining.len().min(65535);
        let is_last = chunk_size == remaining.len();
        let bfinal: u8 = if is_last { 1 } else { 0 };
        // BFINAL + BTYPE=00 packed in first byte
        out.push(bfinal); // BFINAL=1, BTYPE=00
        let len = chunk_size as u16;
        let nlen = !len;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&nlen.to_le_bytes());
        out.extend_from_slice(&remaining[..chunk_size]);
        remaining = &remaining[chunk_size..];
    }
    out
}

/// CRC32 (ISO 3309)
fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xffff_ffff;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xedb8_8320;
            } else {
                crc >>= 1;
            }
        }
    }
    crc ^ 0xffff_ffff
}
