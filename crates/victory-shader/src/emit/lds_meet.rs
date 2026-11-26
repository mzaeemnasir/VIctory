//! Where the two host waves of a console wave64 program meet around its LDS accesses.

use std::collections::HashMap;

use crate::gfx10::{Fields, Operand};
use crate::ir::{Inst, Reg, Value};
use crate::structure::{LaneExit, Structured};

/// LDS accesses of a console wave run as two host waves, where some sit in blocks that only
/// some lanes run (`all` is [`all_lane_blocks`]): per IR block, whether the host waves meet (a
/// group barrier) at its start and before its exit. `None` if barriers cannot keep the
/// console's order.
///
/// The console runs the wave's lanes in step, so an LDS write is seen by every later read. Two
/// host waves keep that order if they meet between any two accesses of which one writes. A
/// barrier must be run by every thread, so it cannot stand in a lane branch's region; but
/// every way into and out of such a region passes a block all lanes run, and the waves meet
/// there: before the exit of an all-lane block into a region with LDS accesses, and at the
/// start of an all-lane block entered from one. Accesses in all-lane blocks get their own
/// barrier as before. What that cannot order is a write and a read inside one region (blocks
/// joined without an all-lane block between them): refused.
/// shortcut: two writes in one region are taken not to hit the same dword from both halves.
pub(super) fn lds_meetings(p: &crate::ir::Program, st: &Structured, all: &[bool]) -> Option<Vec<(bool, bool)>> {
    let n = st.exits.len();
    let succ = |b: usize| -> Vec<usize> {
        match st.exits[b] {
            LaneExit::Jump(t) => vec![t],
            LaneExit::Branch { taken, not_taken, .. } => vec![taken, not_taken],
            LaneExit::End | LaneExit::Indirect => Vec::new(),
        }
    };
    // regions: blocks not every lane runs, joined by the edges between them
    let mut region: Vec<usize> = (0..n).collect();
    fn find(region: &mut [usize], mut b: usize) -> usize {
        while region[b] != b {
            region[b] = region[region[b]];
            b = region[b];
        }
        b
    }
    for b in 0..n {
        for t in succ(b) {
            if !all[b] && !all[t] {
                let (x, y) = (find(&mut region, b), find(&mut region, t));
                region[x] = y;
            }
        }
    }
    // per region: LDS reads, LDS writes
    let mut reads = vec![false; n];
    let mut writes = vec![false; n];
    for (b, block) in p.blocks.iter().enumerate().take(n) {
        if all[b] {
            continue;
        }
        let r = find(&mut region, b);
        for i in &block.insts {
            let name = i.name();
            if !matches!(i.fields, Fields::Ds { .. }) || name.starts_with("ds_swizzle") || name.contains("permute") {
                continue;
            }
            if name.starts_with("ds_read") {
                reads[r] = true;
            } else {
                writes[r] = true;
            }
        }
    }
    if (0..n).any(|r| reads[r] && writes[r]) {
        // A region whose writes are all atomics, and whose reads touch none of the bytes those
        // atomics do, reads only what was written before it (past a meeting): its atomics are
        // ordered among themselves by the hardware, as atomics-only regions are taken to be.
        let defs: HashMap<Value, &Inst> = p
            .blocks
            .iter()
            .flat_map(|b| b.insts.iter())
            .flat_map(|i| i.writes.iter().map(move |w| (w.1, i)))
            .collect();
        let mut kinds: HashMap<usize, Vec<(bool, Option<Vec<(u32, u32)>>)>> = HashMap::new();
        for (b, block) in p.blocks.iter().enumerate().take(n) {
            if all[b] {
                continue;
            }
            let r = find(&mut region, b);
            if !(reads[r] && writes[r]) {
                continue;
            }
            for i in &block.insts {
                let name = i.name();
                if !matches!(i.fields, Fields::Ds { .. }) || name.starts_with("ds_swizzle") || name.contains("permute") {
                    continue;
                }
                let read = name.starts_with("ds_read");
                if name.starts_with("ds_write") {
                    if std::env::var_os("VICTORY_WAVE_WHY").is_some() {
                        eprintln!("WAVE64-LDS region {r}: plain write {name} at {:#x}", i.machine.offset);
                    }
                    return None;
                }
                kinds.entry(r).or_default().push((read, lds_constant_bytes(i, &defs)));
            }
        }
        for (r, ops) in &kinds {
            let (reads, atomics): (Vec<_>, Vec<_>) = ops.iter().partition(|(read, _)| *read);
            let disjoint = reads.iter().all(|(_, rb)| {
                atomics.iter().all(|(_, ab)| match (rb, ab) {
                    (Some(rb), Some(ab)) => rb.iter().all(|&(a, b)| ab.iter().all(|&(c, d)| b <= c || d <= a)),
                    _ => false,
                })
            });
            if !disjoint {
                if std::env::var_os("VICTORY_WAVE_WHY").is_some() {
                    eprintln!("WAVE64-LDS region {r}: reads {reads:?} beside atomics {atomics:?}");
                }
                return None;
            }
        }
    }
    let mut meet = vec![(false, false); n];
    for b in 0..n {
        for t in succ(b) {
            if all[b] && !all[t] {
                let r = find(&mut region, t);
                meet[b].1 |= reads[r] || writes[r];
            } else if !all[b] && all[t] {
                let r = find(&mut region, b);
                meet[t].0 |= reads[r] || writes[r];
            }
        }
    }
    Some(meet)
}

/// The LDS bytes `i` touches, `[from, to)` per dword range it names, when its address register
/// holds a constant (a `v_mov_b32` of one); `None` when the address is not known, as for the
/// accesses that take theirs from the lane (`ds_read_addtid_b32`).
fn lds_constant_bytes(i: &Inst, defs: &HashMap<Value, &Inst>) -> Option<Vec<(u32, u32)>> {
    let Fields::Ds { addr: Some(Operand::Vector { reg, .. }), offset0, offset1, gds: false, .. } = i.fields else {
        return None;
    };
    let d = defs.get(&i.read(Reg::V(reg))?)?;
    let Fields::Valu(f) = &d.fields else {
        return None;
    };
    if d.name() != "v_mov_b32" || f.neg | f.abs != 0 {
        return None;
    }
    let base = match *f.src.first()? {
        Operand::Const(c @ 128..=192) => u32::from(c) - 128,
        Operand::Literal(x) => x,
        _ => return None,
    };
    let name = i.name();
    // The data type's part of the name (`b32`, `u64`, `u8` of `ds_read_u8_d16_hi`...).
    let bits: u32 = name
        .split('_')
        .find_map(|part| {
            let rest = part.strip_prefix(['b', 'u', 'i', 'f'])?;
            rest.parse().ok().filter(|&b: &u32| b >= 8)
        })?;
    let size = bits / 8;
    // The two-address forms (`ds_read2_b32`, `ds_wrxchg2st64_rtn_b64`...) scale each offset by
    // the size, 64 times for `st64`; the others take one 16-bit byte offset.
    let pair = ["read2", "write2", "wrxchg2", "cmpst2"].iter().any(|s| name.contains(s));
    let ranges = if pair {
        let step = if name.contains("st64") { size * 64 } else { size };
        [offset0, offset1]
            .iter()
            .map(|&o| {
                let at = base.checked_add(u32::from(o) * step)?;
                Some((at, at.checked_add(size)?))
            })
            .collect::<Option<Vec<_>>>()?
    } else {
        let at = base.checked_add(u32::from(offset0) | u32::from(offset1) << 8)?;
        vec![(at, at.checked_add(size)?)]
    };
    Some(ranges)
}


#[cfg(test)]
mod tests {
    use crate::ir;

    const S_ENDPGM: u32 = 0xBF81_0000;

    #[test]
    fn lds_accesses_at_constant_addresses_name_their_bytes() {
        use std::collections::HashMap;
        let code: Vec<u8> = [
            0x7E02_0290u32, // v_mov_b32 v1, 16
            0xD800_0004,    // ds_add_u32 v1, v2 offset:4
            0x0000_0201,
            0xD8DC_0201, // ds_read2_b32 v[3:4], v1 offset0:1 offset1:2
            0x0300_0001,
            0xD800_0000, // ds_add_u32 v5, v2 (v5 holds no constant)
            0x0000_0205,
            S_ENDPGM,
        ]
        .iter()
        .flat_map(|w| w.to_le_bytes())
        .collect();
        let p = ir::build(&code, true, 256).unwrap();
        let insts: Vec<_> = p.blocks.iter().flat_map(|b| b.insts.iter()).collect();
        let defs: HashMap<_, _> = insts.iter().flat_map(|i| i.writes.iter().map(move |w| (w.1, *i))).collect();
        let ds: Vec<_> = insts.iter().filter(|i| i.name().starts_with("ds_")).collect();
        assert_eq!(ds.iter().map(|i| i.name()).collect::<Vec<_>>(), ["ds_add_u32", "ds_read2_b32", "ds_add_u32"]);
        assert_eq!(super::lds_constant_bytes(ds[0], &defs), Some(vec![(20, 24)]));
        assert_eq!(super::lds_constant_bytes(ds[1], &defs), Some(vec![(20, 24), (24, 28)]));
        assert_eq!(super::lds_constant_bytes(ds[2], &defs), None);
    }
}
