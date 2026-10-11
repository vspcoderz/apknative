//! DEX (Dalvik Executable) parser.
//!
//! Layout follows the Dalvik executable format: a fixed 112-byte header, then
//! ID tables (strings, types, protos, fields, methods, class defs) that are all
//! offset-based into the variable-size data section.

pub type StrIdx = u32;
pub type TypeIdx = u32;
pub type MethodIdx = u32;
pub type FieldIdx = u32;

pub const ACC_STATIC: u32 = 0x0008;
pub const ACC_NATIVE: u32 = 0x0100;

#[derive(Debug, Clone)]
pub struct Proto {
    pub shorty: StrIdx,
    pub ret: TypeIdx,
    pub params: Vec<TypeIdx>,
}

#[derive(Debug, Clone)]
pub struct FieldId {
    pub class: u16,
    pub ty: u16,
    pub name: StrIdx,
}

#[derive(Debug, Clone)]
pub struct MethodId {
    pub class: u16,
    pub proto: u16,
    pub name: StrIdx,
}

#[derive(Debug, Clone)]
pub struct ClassDef {
    pub class_idx: TypeIdx,
    pub access_flags: u32,
    pub superclass_idx: TypeIdx,
    pub interfaces_off: u32,
    pub source_file_idx: StrIdx,
    pub class_data_off: u32,
}

#[derive(Debug, Clone)]
pub struct EncodedMethod {
    pub method_idx: MethodIdx,
    pub access_flags: u32,
    pub code_off: u32,
}

#[derive(Debug, Clone, Default)]
pub struct ClassData {
    pub static_fields: Vec<(FieldIdx, u32)>,
    pub instance_fields: Vec<(FieldIdx, u32)>,
    pub direct_methods: Vec<EncodedMethod>,
    pub virtual_methods: Vec<EncodedMethod>,
}

#[derive(Debug, Clone)]
pub struct CodeItem {
    pub registers_size: u16,
    pub ins_size: u16,
    pub outs_size: u16,
    pub tries_size: u16,
    pub insns: Vec<u16>,
}

/// Bounds-checked little-endian reader.
struct R<'a> {
    d: &'a [u8],
    p: usize,
}

impl<'a> R<'a> {
    fn new(d: &'a [u8], p: usize) -> Self {
        R { d, p }
    }
    fn need(&self, n: usize) -> Result<(), String> {
        if self.p + n > self.d.len() {
            Err(format!("read past end of DEX at 0x{:x}", self.p))
        } else {
            Ok(())
        }
    }
    fn u8(&mut self) -> Result<u8, String> {
        self.need(1)?;
        let v = self.d[self.p];
        self.p += 1;
        Ok(v)
    }
    fn u16(&mut self) -> Result<u16, String> {
        self.need(2)?;
        let v = u16::from_le_bytes([self.d[self.p], self.d[self.p + 1]]);
        self.p += 2;
        Ok(v)
    }
    fn u32(&mut self) -> Result<u32, String> {
        self.need(4)?;
        let v = u32::from_le_bytes([
            self.d[self.p],
            self.d[self.p + 1],
            self.d[self.p + 2],
            self.d[self.p + 3],
        ]);
        self.p += 4;
        Ok(v)
    }
    fn uleb128(&mut self) -> Result<u32, String> {
        let mut result: u32 = 0;
        let mut shift = 0;
        for i in 0..5 {
            let b = self.u8()?;
            result |= ((b & 0x7f) as u32) << shift;
            if b & 0x80 == 0 {
                return Ok(result);
            }
            shift += 7;
            if i == 4 {
                return Err("uleb128 longer than 5 bytes".into());
            }
        }
        Err("uleb128 longer than 5 bytes".into())
    }
}

pub struct Dex {
    pub strings: Vec<String>,
    pub types: Vec<TypeIdx>,
    pub protos: Vec<Proto>,
    pub fields: Vec<FieldId>,
    pub methods: Vec<MethodId>,
    pub classes: Vec<ClassDef>,
    pub data: Vec<u8>,
    pub version: String,
}

impl Dex {
    pub fn parse(data: Vec<u8>) -> Result<Dex, String> {
        if data.len() < 112 {
            return Err("DEX smaller than its header".into());
        }
        if &data[0..4] != b"dex\n" {
            return Err("not a DEX file (bad magic)".into());
        }
        let version = String::from_utf8_lossy(&data[4..7]).into_owned();
        if data[7] != 0 {
            return Err("bad DEX magic".into());
        }

        let mut h = R::new(&data, 0x28);
        let endian = h.u32()?;
        if endian != 0x1234_5678 {
            return Err("unsupported byte-swapped DEX".into());
        }

        // link_size, link_off, map_off — unused, but they sit ahead of the
        // string table and must be stepped over to land on the right offset.
        let _link_size = h.u32()?;
        let _link_off = h.u32()?;
        let _map_off = h.u32()?;

        let string_ids_size = h.u32()? as usize;
        let string_ids_off = h.u32()? as usize;
        let type_ids_size = h.u32()? as usize;
        let type_ids_off = h.u32()? as usize;
        let proto_ids_size = h.u32()? as usize;
        let proto_ids_off = h.u32()? as usize;
        let field_ids_size = h.u32()? as usize;
        let field_ids_off = h.u32()? as usize;
        let method_ids_size = h.u32()? as usize;
        let method_ids_off = h.u32()? as usize;
        let class_defs_size = h.u32()? as usize;
        let class_defs_off = h.u32()? as usize;

        // string_ids: u32 offsets into the data section
        let mut r = R::new(&data, string_ids_off);
        let mut string_offs = Vec::with_capacity(string_ids_size);
        for _ in 0..string_ids_size {
            string_offs.push(r.u32()? as usize);
        }

        let mut strings = Vec::with_capacity(string_ids_size);
        for &off in &string_offs {
            let mut r = R::new(&data, off);
            let _utf16_size = r.uleb128()?;
            // MUTF-8 bytes, NUL-terminated. Real MUTF-8 encodes U+0000 as
            // 0xC0 0x80, which `from_utf8_lossy` renders as garbage but never
            // crashes on — good enough for now.
            let start = r.p;
            let mut end = start;
            while end < data.len() && data[end] != 0 {
                end += 1;
            }
            strings.push(String::from_utf8_lossy(&data[start..end]).into_owned());
        }

        // type_ids: u32 indices into string_ids
        let mut r = R::new(&data, type_ids_off);
        let mut types = Vec::with_capacity(type_ids_size);
        for _ in 0..type_ids_size {
            types.push(r.u32()?);
        }

        // proto_ids: 12 bytes each
        let mut r = R::new(&data, proto_ids_off);
        let mut protos = Vec::with_capacity(proto_ids_size);
        for _ in 0..proto_ids_size {
            let shorty = r.u32()?;
            let ret = r.u32()?;
            let params_off = r.u32()? as usize;
            let params = if params_off == 0 {
                Vec::new()
            } else {
                let mut pr = R::new(&data, params_off);
                let size = pr.u32()? as usize;
                let mut v = Vec::with_capacity(size);
                for _ in 0..size {
                    v.push(pr.u16()? as TypeIdx);
                }
                v
            };
            protos.push(Proto { shorty, ret, params });
        }

        // field_ids: 8 bytes each
        let mut r = R::new(&data, field_ids_off);
        let mut fields = Vec::with_capacity(field_ids_size);
        for _ in 0..field_ids_size {
            fields.push(FieldId {
                class: r.u16()?,
                ty: r.u16()?,
                name: r.u32()?,
            });
        }

        // method_ids: 8 bytes each
        let mut r = R::new(&data, method_ids_off);
        let mut methods = Vec::with_capacity(method_ids_size);
        for _ in 0..method_ids_size {
            methods.push(MethodId {
                class: r.u16()?,
                proto: r.u16()?,
                name: r.u32()?,
            });
        }

        // class_defs: 32 bytes each
        let mut r = R::new(&data, class_defs_off);
        let mut classes = Vec::with_capacity(class_defs_size);
        for _ in 0..class_defs_size {
            let class_idx = r.u32()?;
            let access_flags = r.u32()?;
            let superclass_idx = r.u32()?;
            let interfaces_off = r.u32()?;
            let source_file_idx = r.u32()?;
            let _annotations_off = r.u32()?;
            let class_data_off = r.u32()?;
            let _static_values_off = r.u32()?;
            classes.push(ClassDef {
                class_idx,
                access_flags,
                superclass_idx,
                interfaces_off,
                source_file_idx,
                class_data_off,
            });
        }

        Ok(Dex {
            strings,
            types,
            protos,
            fields,
            methods,
            classes,
            data,
            version,
        })
    }

    pub fn string(&self, i: StrIdx) -> &str {
        self.strings.get(i as usize).map(|s| s.as_str()).unwrap_or("<?>")
    }

    /// Human-readable form of a type descriptor: `Ljava/lang/String;` -> `java/lang/String`
    pub fn type_name(&self, i: TypeIdx) -> &str {
        self.strings
            .get(self.types.get(i as usize).copied().unwrap_or(0) as usize)
            .map(|s| s.as_str())
            .unwrap_or("<?>")
    }

    /// Type descriptor without the `L...;` wrapper, as used for class names.
    /// Uses single-character strip, not `trim_*_matches`, because a class in
    /// the default package has the descriptor `LLit;`.
    pub fn class_name(&self, i: TypeIdx) -> String {
        let d = self.type_name(i);
        let inner = d.strip_prefix('L').unwrap_or(d);
        inner.strip_suffix(';').unwrap_or(inner).to_string()
    }

    pub fn proto(&self, i: u16) -> Option<&Proto> {
        self.protos.get(i as usize)
    }



    pub fn class_def(&self, type_idx: TypeIdx) -> Option<&ClassDef> {
        self.classes.iter().find(|c| c.class_idx == type_idx)
    }

    pub fn class_data(&self, off: u32) -> Result<ClassData, String> {
        // A zero offset means "no class data" — parsing at 0 would read the DEX
        // magic as uleb128 counts and produce nonsense indices.
        if off == 0 {
            return Ok(ClassData::default());
        }
        let mut r = R::new(&self.data, off as usize);
        let static_fields_size = r.uleb128()?;
        let instance_fields_size = r.uleb128()?;
        let direct_methods_size = r.uleb128()?;
        let virtual_methods_size = r.uleb128()?;

        let mut cd = ClassData {
            static_fields: Vec::with_capacity(static_fields_size as usize),
            instance_fields: Vec::with_capacity(instance_fields_size as usize),
            direct_methods: Vec::with_capacity(direct_methods_size as usize),
            virtual_methods: Vec::with_capacity(virtual_methods_size as usize),
        };

        // Each table stores *deltas* from the previous index, reset per table.
        acc_fields(&mut r, static_fields_size, &mut cd.static_fields, self.fields.len())?;
        acc_fields(
            &mut r,
            instance_fields_size,
            &mut cd.instance_fields,
            self.fields.len(),
        )?;
        acc_methods(
            &mut r,
            direct_methods_size,
            &mut cd.direct_methods,
            self.methods.len(),
        )?;
        acc_methods(
            &mut r,
            virtual_methods_size,
            &mut cd.virtual_methods,
            self.methods.len(),
        )?;

        Ok(cd)
    }

    pub fn code_item(&self, off: u32) -> Result<CodeItem, String> {
        let mut r = R::new(&self.data, off as usize);
        let registers_size = r.u16()?;
        let ins_size = r.u16()?;
        let outs_size = r.u16()?;
        let tries_size = r.u16()?;
        let _debug_info_off = r.u32()?;
        let insns_size = r.u32()? as usize;
        let mut insns = Vec::with_capacity(insns_size);
        for _ in 0..insns_size {
            insns.push(r.u16()?);
        }
        // Pad so a few wide-format instructions at the tail can't panic.
        insns.resize(insns.len() + 8, 0);
        Ok(CodeItem {
            registers_size,
            ins_size,
            outs_size,
            tries_size,
            insns,
        })
    }

    /// Descriptor of a method's return type, e.g. `V`, `I`, `Ljava/lang/String;`
    pub fn return_type(&self, m: &MethodId) -> &str {
        self.proto(m.proto)
            .map(|p| self.type_name(p.ret))
            .unwrap_or("V")
    }

}

fn acc_fields(
    r: &mut R,
    count: u32,
    out: &mut Vec<(FieldIdx, u32)>,
    max: usize,
) -> Result<(), String> {
    let mut acc: FieldIdx = 0;
    for _ in 0..count {
        let diff = r.uleb128()?;
        let flags = r.uleb128()?;
        acc = acc.wrapping_add(diff);
        if acc as usize >= max {
            return Err(format!("field index {acc} out of range ({max})"));
        }
        out.push((acc, flags));
    }
    Ok(())
}

fn acc_methods(
    r: &mut R,
    count: u32,
    out: &mut Vec<EncodedMethod>,
    max: usize,
) -> Result<(), String> {
    let mut acc: MethodIdx = 0;
    for _ in 0..count {
        let diff = r.uleb128()?;
        let flags = r.uleb128()?;
        let code_off = r.uleb128()?;
        acc = acc.wrapping_add(diff);
        if acc as usize >= max {
            return Err(format!("method index {acc} out of range ({max})"));
        }
        out.push(EncodedMethod {
            method_idx: acc,
            access_flags: flags,
            code_off,
        });
    }
    Ok(())
}