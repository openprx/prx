#![cfg(test)]

use super::{ROUTER_HISTORY_MAX_DIMENSIONS, decode_vector};
use anyhow::Context as _;

#[test]
fn history_vector_decode_preserves_every_finite_bit_at_capacity_boundaries() -> anyhow::Result<()> {
    let bits = [
        0,
        0x8000_0000,
        1,
        0x8000_0001,
        0x7f7f_ffff,
        0xff7f_ffff,
        0x3f80_0000_u32,
    ];
    for dimensions in [1_u16, 2, 127, 128, 129, 384, 2048] {
        let expected: Vec<_> = bits.into_iter().cycle().take(usize::from(dimensions)).collect();
        let bytes: Vec<_> = expected.iter().flat_map(|value| value.to_le_bytes()).collect();
        let decoded = decode_vector(&bytes, dimensions)?;
        assert_eq!(
            decoded.iter().map(|value| value.to_bits()).collect::<Vec<_>>(),
            expected
        );
    }
    Ok(())
}

#[test]
fn history_vector_decode_rejects_bad_lengths_dimensions_and_nonfinite_values() -> anyhow::Result<()> {
    assert!(decode_vector(&[], 0).is_err());
    assert!(decode_vector(&[0; 4], 2).is_err());
    assert!(decode_vector(&[0; 5], 1).is_err());
    let over_capacity = u16::try_from(ROUTER_HISTORY_MAX_DIMENSIONS + 1)?;
    assert!(decode_vector(&vec![0; usize::from(over_capacity) * 4], over_capacity).is_err());
    for nonfinite in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        for index in [0, 192, 383] {
            let mut bytes = vec![0; 384 * 4];
            bytes
                .get_mut(index * 4..(index + 1) * 4)
                .context("fixture vector slot")?
                .copy_from_slice(&nonfinite.to_le_bytes());
            assert!(decode_vector(&bytes, 384).is_err());
        }
    }
    Ok(())
}
