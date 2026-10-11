//! Static consistency checks over a DEX's bytecode.
//!
//! Finding decoder bugs one runtime panic at a time against a real APK does not
//! scale: instruction widths, branch targets and register accesses all have to
//! be right before any app code can run. This walks every method up front and
//! reports everything that is wrong.

use crate::dex::Dex;
use crate::runtime::insn_width;

/// Check every method, returning a human-readable list of problems.
pub fn verify(dex: &Dex) -> Vec<String> {
    let mut errs = Vec::new();

    for class in &dex.classes {
        let cname = dex.class_name(class.class_idx);
        let data = match dex.class_data(class.class_data_off) {
            Ok(d) => d,
            Err(e) => {
                errs.push(format!("{cname}: class_data: {e}"));
                continue;
            }
        };

        for em in data.direct_methods.iter().chain(data.virtual_methods.iter()) {
            if em.code_off == 0 {
                continue;
            }
            let mid = &dex.methods[em.method_idx as usize];
            let label = format!("{cname}.{}", dex.string(mid.name));
            let code = match dex.code_item(em.code_off) {
                Ok(c) => c,
                Err(e) => {
                    errs.push(format!("{label}: code_item: {e}"));
                    continue;
                }
            };

            let nregs = code.registers_size as usize;
            // The parser pads `insns` by 8 words so a wide tail instruction
            // can't panic; those aren't real instructions.
            let len = code.insns.len().saturating_sub(8);
            let insns = &code.insns[..len];

            // Pass 1 — every instruction must fit; record the boundaries.
            let mut bounds = vec![false; len + 1];
            let mut saw_payload = false;
            let mut pc = 0usize;
            while pc < len {
                let op = (insns[pc] & 0xff) as u8;
                let w = insn_width(op);
                if w == 0 || pc + w > len {
                    errs.push(format!(
                        "{label}: opcode 0x{op:02x} at 0x{pc:04x} claims {w} units, {} available",
                        len - pc
                    ));
                    break;
                }
                bounds[pc] = true;
                pc += w;
                // Inline data payloads are not instructions, and their length
                // depends on the array's element width which isn't recorded in
                // the instruction. Mark the rest of the method as unverifiable
                // rather than reporting every branch past the payload.
                if matches!(op, 0x26 | 0x2b | 0x2c) {
                    saw_payload = true;
                    break;
                }
            }
            bounds[len] = true;

            // Pass 2 — branches must land on a boundary; registers must be in range.
            let mut pc = 0usize;
            while pc < len {
                let unit = insns[pc];
                let op = (unit & 0xff) as u8;
                let w = insn_width(op);
                // Pass 1 may have stopped early; don't read past the body.
                if pc + w > len {
                    break;
                }

                let check_branch = |off: i32, what: &str, errs: &mut Vec<String>| {
                    let t = pc as i32 + off;
                    if t < 0 || t as usize >= bounds.len() {
                        errs.push(format!(
                            "{label}: {what} at 0x{pc:04x} targets 0x{t:x}, outside the method"
                        ));
                    } else if !bounds[t as usize] {
                        errs.push(format!(
                            "{label}: {what} at 0x{pc:04x} targets 0x{t:x}, not an instruction boundary"
                        ));
                    }
                };
                if saw_payload {
                    // Boundaries past an inline payload are unknown, so only
                    // range checks remain meaningful.
                } else {
                match op {
                    0x28 => check_branch(((unit >> 8) & 0xff) as i8 as i32, "goto", &mut errs),
                    0x29 => check_branch(insns[pc + 1] as i16 as i32, "goto/16", &mut errs),
                    0x2a => check_branch(
                        (insns[pc + 1] as i32) | ((insns[pc + 2] as i32) << 16),
                        "goto/32",
                        &mut errs,
                    ),
                    0x32..=0x37 => check_branch(insns[pc + 1] as i16 as i32, "if-test", &mut errs),
                    0x38..=0x3d => check_branch(insns[pc + 1] as i16 as i32, "if-testz", &mut errs),
                    _ => {}
                }
                }

                // `a4`/`b4` are the two single-nibble operands of `B|A|op`;
                // `aa` is the full high byte. Which one applies depends on the
                // instruction's format, not the opcode.
                let a4 = ((unit >> 8) & 0x0f) as usize;
                let b4 = ((unit >> 12) & 0x0f) as usize;
                let aa = ((unit >> 8) & 0xff) as usize;
                let next = if pc + 1 < len { insns[pc + 1] as usize } else { 0 };
                let next_hi = if pc + 1 < len { (insns[pc + 1] >> 8) as usize } else { 0 };

                let check_reg = |r: usize, errs: &mut Vec<String>| {
                    if r >= nregs {
                        errs.push(format!(
                            "{label}: opcode 0x{op:02x} at 0x{pc:04x} touches v{r}, frame has {nregs}"
                        ));
                    }
                };

                if saw_payload {
                    pc += w;
                    continue;
                }
                match op {
                    // 12x — two nibble registers (array-length is 12x, not 21x).
                    0x01 | 0x04 | 0x07 | 0x21 | 0x7b..=0x8f | 0xb0..=0xcf => {
                        check_reg(a4, &mut errs);
                        check_reg(b4, &mut errs);
                    }
                    // 11n — nibble destination, nibble literal.
                    0x12 => check_reg(a4, &mut errs),
                    // 22t / 22c / 22s — two nibble registers.
                    0x32..=0x37 | 0x20 | 0x23 | 0x52..=0x5f | 0xd0..=0xd7 => {
                        check_reg(a4, &mut errs);
                        check_reg(b4, &mut errs);
                    }
                    // 11x — single AA operand. return-void (10x) has none.
                    0x0a..=0x0d | 0x0f..=0x11 | 0x13 | 0x15 | 0x16 | 0x19 | 0x1a | 0x1b
                    | 0x1c | 0x22 => check_reg(aa, &mut errs),
                    // 22x — AA plus a full register in the next unit.
                    0x02 | 0x05 | 0x08 => {
                        check_reg(aa, &mut errs);
                        if pc + 1 < len {
                            check_reg(next, &mut errs);
                        }
                    }
                    // 23x — AA plus two byte-sized registers packed in one unit.
                    0x2d..=0x31 | 0x44..=0x51 | 0x90..=0xaf => {
                        check_reg(aa, &mut errs);
                        if pc + 1 < len {
                            check_reg(next & 0xff, &mut errs);
                            check_reg(next_hi, &mut errs);
                        }
                    }
                    // 22b — AA destination; the next unit packs (literal<<8)|register.
                    0xd8..=0xe2 => {
                        check_reg(aa, &mut errs);
                        if pc + 1 < len {
                            check_reg(next & 0xff, &mut errs);
                        }
                    }
                    // 21c — AA plus an index (field/string/type), not a register.
                    0x60..=0x6d => check_reg(aa, &mut errs),
                    // 32x — two full 16-bit registers.
                    0x03 | 0x06 | 0x09
                        if pc + 2 < len => {
                            check_reg(insns[pc + 1] as usize, &mut errs);
                            check_reg(insns[pc + 2] as usize, &mut errs);
                        }
                    _ => {}
                }

                pc += w;
            }
        }
    }
    errs
}