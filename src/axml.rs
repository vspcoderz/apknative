//! Android binary XML (AXML) parser — enough to read `AndroidManifest.xml`.
//!
//! Real APKs store the manifest in Android's binary XML format: a string pool
//! followed by a stream of XML event chunks. This decodes the tree so we can
//! find the launcher activity, which is what an app actually starts with (APKs
//! have no `main`).

const RES_STRING_POOL_TYPE: u16 = 0x0001;
const RES_XML_START_ELEMENT_TYPE: u16 = 0x0102;
const RES_XML_END_ELEMENT_TYPE: u16 = 0x0103;
const RES_XML_START_NAMESPACE_TYPE: u16 = 0x0100;
const RES_XML_END_NAMESPACE_TYPE: u16 = 0x0101;
const RES_XML_RESOURCE_MAP_TYPE: u16 = 0x0180;

const UTF8_FLAG: u32 = 1 << 8;

#[derive(Debug, Clone)]
pub struct Node {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Node>,
}

impl Node {
    pub fn attr(&self, key: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
    pub fn find_all(&self, name: &str) -> Vec<&Node> {
        self.children.iter().filter(|c| c.name == name).collect()
    }
}

fn u16le(d: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([d[o], d[o + 1]])
}
fn u32le(d: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]])
}

fn safe(d: &[u8], o: usize, n: usize) -> Result<&[u8], String> {
    d.get(o..o + n)
        .ok_or_else(|| format!("AXML read past end at 0x{o:x}"))
}

/// Decode the manifest's string pool into owned strings.
fn parse_string_pool(d: &[u8]) -> Result<Vec<String>, String> {
    if d.len() < 28 {
        return Err("AXML too small".into());
    }
    let string_count = u32le(d, 8) as usize;
    let flags = u32le(d, 16);
    let strings_start = u32le(d, 20) as usize;
    let is_utf8 = flags & UTF8_FLAG != 0;

    let mut out = Vec::with_capacity(string_count);
    for i in 0..string_count {
        let off_ptr = 28 + 4 * i;
        let entry = u32le(d, off_ptr) as usize;
        let p = strings_start + entry;
        if p >= d.len() {
            out.push(String::new());
            continue;
        }
        let s = if is_utf8 {
            // Two length fields (chars, then bytes), each 1 or 2 bytes.
            let mut q = p;
            let skip_len = |d: &[u8], q: &mut usize| {
                let a = d[*q];
                if a & 0x80 != 0 {
                    *q += 2;
                } else {
                    *q += 1;
                }
            };
            skip_len(d, &mut q);
            skip_len(d, &mut q);
            let start = q;
            let mut end = start;
            while end < d.len() && d[end] != 0 {
                end += 1;
            }
            String::from_utf8_lossy(&d[start..end]).into_owned()
        } else {
            // UTF-16LE, length in u16 units, NUL-terminated.
            let len = u16le(d, p) as usize;
            let start = p + 2;
            let units = safe(d, start, len * 2)?;
            String::from_utf16_lossy(
                &units
                    .as_chunks::<2>().0.iter()
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect::<Vec<u16>>(),
            )
        };
        out.push(s);
    }
    Ok(out)
}

fn pool_string(pool: &[String], idx: u32) -> String {
    if idx == u32::MAX {
        return String::new();
    }
    pool.get(idx as usize).cloned().unwrap_or_default()
}

/// Format an attribute's typed value when it isn't a plain string.
fn typed_value(d: &[u8], attr_off: usize, pool: &[String]) -> String {
    let raw = u32le(d, attr_off + 8);
    if raw != u32::MAX {
        return pool_string(pool, raw);
    }
    let data_type = d[attr_off + 15];
    let data = u32le(d, attr_off + 16);
    match data_type {
        0x03 => pool_string(pool, data), // TYPE_STRING
        0x12 => {
            // TYPE_BOOLEAN is stored as 0xffffffff / 0x00000000, not as a mask.
            if data != 0 { "true".into() } else { "false".into() }
        }
        0x10 => (data as i32).to_string(), // TYPE_INT_DEC
        0x11 => (data as i32).to_string(), // TYPE_INT_HEX
        0x01 => {
            // TYPE_REFERENCE — '@' + package:resource form isn't recoverable
            // here, so report the raw id.
            format!("@0x{data:08x}")
        }
        _ => data.to_string(),
    }
}

/// Parse an AXML document into a node tree.
pub fn parse(d: &[u8]) -> Result<Node, String> {
    if d.len() < 8 {
        return Err("AXML too small".into());
    }
    let mut pool: Vec<String> = Vec::new();
    let mut root = Node {
        name: String::from("#document"),
        attrs: Vec::new(),
        children: Vec::new(),
    };
    let mut stack: Vec<Node> = Vec::new();

    let mut off = 8; // skip the file header
    while off + 8 <= d.len() {
        let ctype = u16le(d, off);
        let chunk_size = u32le(d, off + 4) as usize;
        if chunk_size == 0 || off + chunk_size > d.len() {
            break;
        }
        let body = &d[off..off + chunk_size];

        match ctype {
            RES_STRING_POOL_TYPE => {
                pool = parse_string_pool(body)?;
            }
            RES_XML_START_NAMESPACE_TYPE | RES_XML_END_NAMESPACE_TYPE
            | RES_XML_RESOURCE_MAP_TYPE => {}
            RES_XML_START_ELEMENT_TYPE => {
                // Layout: 8-byte chunk header + lineNumber(4) + comment(4) = a
                // 16-byte node header, then attrExt { ns, name, attributeStart,
                // attributeSize, attributeCount, idIndex, classIndex, styleIndex }.
                const NODE_HEADER: usize = 16;
                let name_idx = u32le(body, NODE_HEADER + 4);
                // attributeStart/Size/Count are uint16_t, not uint32_t.
                let attr_start = NODE_HEADER + u16le(body, NODE_HEADER + 8) as usize;
                let attr_size = u16le(body, NODE_HEADER + 10) as usize;
                let attr_count = u16le(body, NODE_HEADER + 12) as usize;

                let mut node = Node {
                    name: pool_string(&pool, name_idx),
                    attrs: Vec::with_capacity(attr_count),
                    children: Vec::new(),
                };
                for i in 0..attr_count {
                    let aoff = attr_start + i * attr_size;
                    if aoff + 20 > body.len() {
                        break;
                    }
                    let aname = pool_string(&pool, u32le(body, aoff + 4));
                    let val = typed_value(body, aoff, &pool);
                    node.attrs.push((aname, val));
                }
                stack.push(node);
            }
            RES_XML_END_ELEMENT_TYPE => {
                if let Some(node) = stack.pop() {
                    match stack.last_mut() {
                        Some(parent) => parent.children.push(node),
                        None => root.children.push(node),
                    }
                }
            }
            _ => {}
        }
        off += chunk_size;
    }

    // Any unclosed nodes (malformed doc) still belong to the tree.
    while let Some(node) = stack.pop() {
        match stack.last_mut() {
            Some(parent) => parent.children.push(node),
            None => root.children.push(node),
        }
    }
    Ok(root)
}

const MAIN_ACTION: &str = "android.intent.action.MAIN";
const LAUNCHER_CATEGORY: &str = "android.intent.category.LAUNCHER";

/// Find the activity Android would launch for this manifest.
///
/// Handles both `<activity android:name="...">` and `<activity-alias>`, and
/// resolves the leading-dot shorthand (".Main" -> "com.pkg.Main") against the
/// manifest's package attribute.
pub fn launcher_activity(root: &Node) -> Option<String> {
    // <application> is a child of <manifest>, which is a child of the document.
    let manifest = root
        .children
        .iter()
        .find(|c| c.name == "manifest")?;
    let package = manifest.attr("package").unwrap_or("").to_string();

    for app in manifest.find_all("application") {
        for kind in ["activity", "activity-alias"] {
            for act in app.find_all(kind) {
                // A disabled component is never launched by the system.
                if act.attr("enabled") == Some("false") {
                    continue;
                }
                let is_launcher = act.find_all("intent-filter").iter().any(|f| {
                    let has_main = f.find_all("action").iter().any(|a| {
                        a.attr("name") == Some(MAIN_ACTION)
                    });
                    let has_cat = f.find_all("category").iter().any(|c| {
                        c.attr("name") == Some(LAUNCHER_CATEGORY)
                    });
                    has_main && has_cat
                });
                if !is_launcher {
                    continue;
                }
                // An alias is not itself a class; its target is.
                let raw = act.attr("targetActivity").or_else(|| act.attr("name"))?;
                let resolved = if let Some(rest) = raw.strip_prefix('.') {
                    format!("{package}{rest}")
                } else if raw.contains('.') {
                    raw.to_string()
                } else {
                    format!("{package}.{raw}")
                };
                return Some(resolved);
            }
        }
    }
    None
}

/// Render the tree as indented text (used by the `manifest` subcommand).
pub fn dump(node: &Node, indent: usize, out: &mut String) {
    if node.name != "#document" {
        let pad = "  ".repeat(indent);
        out.push_str(&pad);
        out.push('<');
        out.push_str(&node.name);
        for (k, v) in &node.attrs {
            out.push_str(&format!(" {k}=\"{v}\""));
        }
        if node.children.is_empty() {
            out.push_str(" />\n");
        } else {
            out.push_str(">\n");
        }
    }
    for c in &node.children {
        dump(c, indent + 1, out);
    }
    if node.name != "#document" && !node.children.is_empty() {
        out.push_str(&"  ".repeat(indent));
        out.push_str(&format!("</{}>\n", node.name));
    }
}