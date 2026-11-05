//! What a module's variables hold at a probe: scopes and types from its DWARF, values from
//! what the probe wrote out and from its memory.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::rc::Rc;

use gimli::Reader as _;
use gimli::{
    AttributeValue, DW_AT_abstract_origin, DW_AT_byte_size, DW_AT_const_value, DW_AT_count,
    DW_AT_data_member_location, DW_AT_discr, DW_AT_discr_value, DW_AT_encoding, DW_AT_frame_base,
    DW_AT_location, DW_AT_name, DW_AT_type, DW_AT_upper_bound, DW_ATE_UTF, DW_ATE_boolean,
    DW_ATE_float, DW_ATE_signed, DW_ATE_signed_char, DW_TAG_array_type, DW_TAG_base_type,
    DW_TAG_const_type, DW_TAG_enumeration_type, DW_TAG_enumerator, DW_TAG_formal_parameter,
    DW_TAG_inlined_subroutine, DW_TAG_lexical_block, DW_TAG_member, DW_TAG_pointer_type,
    DW_TAG_reference_type, DW_TAG_restrict_type, DW_TAG_rvalue_reference_type,
    DW_TAG_structure_type, DW_TAG_subprogram, DW_TAG_subrange_type, DW_TAG_template_type_parameter,
    DW_TAG_typedef, DW_TAG_union_type, DW_TAG_variable, DW_TAG_variant, DW_TAG_variant_part,
    DW_TAG_volatile_type, Dwarf, EndianRcSlice, LittleEndian, Operation, SectionId, Unit,
    UnitOffset,
};
use wasmparser::{Parser, Payload};

use crate::{ModuleOffset, ProbeError};

type Reader = EndianRcSlice<LittleEndian>;
type Entry<'a> = gimli::DebuggingInformationEntry<Reader>;

/// How many nested values are read of a variable.
const MOST_DEPTH: usize = 6;
/// How many items of an array, a list or a slice are read.
const MOST_ITEMS: usize = 64;
/// How many bytes of text are read.
const MOST_TEXT: u32 = 512;
/// How long a one-line summary gets.
const MOST_SUMMARY: usize = 120;
/// How many values are read for all of a stop: what a cycle of pointers cannot get past.
const MOST_VALUES: usize = 4000;

/// What a guest had when it stopped.
pub trait Frame {
    /// A wasm local of the function it stopped in, as its little-endian bytes.
    fn local(&self, index: u32) -> Option<[u8; 8]>;
    /// A wasm global, as its little-endian bytes.
    fn global(&self, index: u32) -> Option<[u8; 8]>;
    /// `len` bytes of its linear memory, if they are all there.
    fn read(&self, address: u32, len: u32) -> Option<Vec<u8>>;
}

/// A variable, or part of one.
#[derive(Clone, Debug, PartialEq)]
pub struct Variable {
    /// Its name: a field's, or an item's index.
    pub name: String,
    /// Its type, as the source has it.
    pub type_name: String,
    /// What it holds, on one line.
    pub value: String,
    /// What it is made of, to look into.
    pub children: Vec<Variable>,
}

/// A module's debug info, kept for reading its variables.
pub struct ModuleDebug {
    dwarf: Dwarf<Reader>,
    code_start: u64,
}

/// The wasm locals and globals a module's debug info reads variables from.
#[derive(Default)]
pub(crate) struct ReadFrom {
    /// By the module offset a function's code starts at, as the debug info gives it.
    locals: BTreeMap<u64, BTreeSet<u32>>,
    pub(crate) globals: BTreeSet<u32>,
}

impl ReadFrom {
    fn note(
        &mut self,
        unit: &Unit<Reader>,
        function: u64,
        expression: &gimli::Expression<Reader>,
    ) -> Result<(), ProbeError> {
        let mut operations = expression.clone().operations(unit.encoding());
        while let Some(operation) = operations.next()? {
            match operation {
                Operation::WasmLocal { index } => {
                    self.locals.entry(function).or_default().insert(index);
                }
                Operation::WasmGlobal { index } => {
                    self.globals.insert(index);
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// The locals read of the function whose body is at `body`, a range of module offsets.
    pub(crate) fn locals_in(&self, body: std::ops::Range<u64>) -> BTreeSet<u32> {
        self.locals
            .range(body)
            .flat_map(|(_, locals)| locals.iter().copied())
            .collect()
    }
}

/// Where a value is.
#[derive(Clone)]
enum Place {
    Memory(u32),
    /// In a wasm local or global, or worked out by the debug info: it has no address.
    Held(Vec<u8>),
}

impl ModuleDebug {
    /// Keeps what `module` says of itself. `None` if it says nothing.
    pub fn new(module: &[u8]) -> Result<Option<Self>, ProbeError> {
        let mut sections: HashMap<&str, &[u8]> = HashMap::new();
        let mut code_start = None;
        for payload in Parser::new(0).parse_all(module) {
            match payload? {
                Payload::CustomSection(section) if section.name().starts_with(".debug_") => {
                    sections.insert(section.name(), section.data());
                }
                Payload::CodeSectionStart {
                    unchecked_range, ..
                } => code_start = Some(unchecked_range.start),
                _ => {}
            }
        }
        let Some(code_start) = code_start else {
            return Ok(None);
        };
        if sections.is_empty() {
            return Ok(None);
        }
        let load = |id: SectionId| -> Result<Reader, gimli::Error> {
            let data = sections.get(id.name()).copied().unwrap_or(&[]);
            Ok(EndianRcSlice::new(Rc::from(data), LittleEndian))
        };
        Ok(Some(Self {
            dwarf: Dwarf::load(load)?,
            code_start,
        }))
    }

    /// What the debug info reads variables from: the rest a probe need not write out, which
    /// would keep them alive in the function, and so slow it even when no probe fires.
    pub(crate) fn read_from(&self) -> Result<ReadFrom, ProbeError> {
        let mut read = ReadFrom::default();
        let mut units = self.dwarf.units();
        while let Some(header) = units.next()? {
            let unit = self.dwarf.unit(header)?;
            let mut tree = unit.entries_tree(None)?;
            self.read_from_in(&unit, tree.root()?, None, &mut read)?;
        }
        Ok(read)
    }

    fn read_from_in(
        &self,
        unit: &Unit<Reader>,
        node: gimli::EntriesTreeNode<'_, '_, Reader>,
        function: Option<u64>,
        read: &mut ReadFrom,
    ) -> Result<(), ProbeError> {
        let entry = node.entry();
        let mut function = function;
        if entry.tag() == DW_TAG_subprogram {
            let mut ranges = self.dwarf.die_ranges(unit, entry)?;
            function = ranges.next()?.map(|range| range.begin + self.code_start);
        }
        if let Some(function) = function {
            for attribute in [DW_AT_frame_base, DW_AT_location] {
                let Some(value) = entry.attr_value(attribute) else {
                    continue;
                };
                if let AttributeValue::Exprloc(expression) = value {
                    read.note(unit, function, &expression)?;
                } else if let Some(mut locations) = self.dwarf.attr_locations(unit, value)? {
                    while let Some(location) = locations.next()? {
                        read.note(unit, function, &location.data)?;
                    }
                }
            }
        }
        let mut children = node.children();
        while let Some(child) = children.next()? {
            self.read_from_in(unit, child, function, read)?;
        }
        Ok(())
    }

    /// The variables in scope at `offset` in the module, innermost scope first. Of two with
    /// one name, as with a shadowed `let`, only the innermost.
    pub fn variables_at(
        &self,
        offset: ModuleOffset,
        frame: &dyn Frame,
    ) -> Result<Vec<Variable>, ProbeError> {
        let pc = u64::from(offset.0).saturating_sub(self.code_start);
        let mut units = self.dwarf.units();
        while let Some(header) = units.next()? {
            let unit = self.dwarf.unit(header)?;
            if !self.unit_covers(&unit, pc)? {
                continue;
            }
            let mut path = Vec::new();
            if self.scopes_at(&unit, pc, &mut path)? {
                let mut reader = Decoder {
                    debug: self,
                    unit: &unit,
                    frame,
                    pc,
                    values: 0,
                };
                return reader.variables(&path);
            }
        }
        Ok(Vec::new())
    }

    fn unit_covers(&self, unit: &Unit<Reader>, pc: u64) -> Result<bool, ProbeError> {
        let mut ranges = self.dwarf.unit_ranges(unit)?;
        while let Some(range) = ranges.next()? {
            if range.begin <= pc && pc < range.end {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn entry_covers(
        &self,
        unit: &Unit<Reader>,
        entry: &Entry<'_>,
        pc: u64,
    ) -> Result<bool, ProbeError> {
        let mut ranges = self.dwarf.die_ranges(unit, entry)?;
        while let Some(range) = ranges.next()? {
            if range.begin <= pc && pc < range.end {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Fills `path` with the scopes around `pc`, outermost first: the offset of a function,
    /// then of each block and inlined call in it.
    fn scopes_at(
        &self,
        unit: &Unit<Reader>,
        pc: u64,
        path: &mut Vec<UnitOffset>,
    ) -> Result<bool, ProbeError> {
        let mut tree = unit.entries_tree(None)?;
        let root = tree.root()?;
        self.descend(unit, root, pc, path)?;
        Ok(!path.is_empty())
    }

    fn descend(
        &self,
        unit: &Unit<Reader>,
        node: gimli::EntriesTreeNode<'_, '_, Reader>,
        pc: u64,
        path: &mut Vec<UnitOffset>,
    ) -> Result<(), ProbeError> {
        let mut children = node.children();
        while let Some(child) = children.next()? {
            let entry = child.entry();
            let tag = entry.tag();
            let offset = entry.offset();
            if tag == DW_TAG_subprogram
                || tag == DW_TAG_lexical_block
                || tag == DW_TAG_inlined_subroutine
            {
                if self.entry_covers(unit, entry, pc)? {
                    path.push(offset);
                    return self.descend(unit, child, pc, path);
                }
            } else if path.is_empty() {
                // Functions may sit in namespaces and types.
                let before = path.len();
                self.descend(unit, child, pc, path)?;
                if path.len() > before {
                    return Ok(());
                }
            }
        }
        Ok(())
    }
}

/// Reads one stop's variables.
struct Decoder<'a> {
    debug: &'a ModuleDebug,
    unit: &'a Unit<Reader>,
    frame: &'a dyn Frame,
    pc: u64,
    values: usize,
}

impl Decoder<'_> {
    fn variables(&mut self, path: &[UnitOffset]) -> Result<Vec<Variable>, ProbeError> {
        // The frame base is the function's, whatever is inlined in it.
        let mut frame_base = None;
        for offset in path {
            let entry = self.unit.entry(*offset)?;
            if entry.tag() == DW_TAG_subprogram {
                frame_base = match entry.attr_value(DW_AT_frame_base) {
                    Some(AttributeValue::Exprloc(expression)) => {
                        self.evaluate(&expression, None).ok().flatten()
                    }
                    _ => None,
                };
            }
        }
        let base = match frame_base {
            Some(Place::Memory(address)) => Some(address),
            Some(Place::Held(bytes)) => bytes
                .get(..4)
                .and_then(|bytes| bytes.try_into().ok())
                .map(u32::from_le_bytes),
            None => None,
        };

        let mut found: Vec<Variable> = Vec::new();
        for offset in path.iter().rev() {
            let entries = self.children_of(*offset)?;
            for child in entries {
                let entry = self.unit.entry(child)?;
                if entry.tag() != DW_TAG_variable && entry.tag() != DW_TAG_formal_parameter {
                    continue;
                }
                let Some(name) = self.name_of(&entry)? else {
                    continue;
                };
                if found.iter().any(|variable| variable.name == name) {
                    continue;
                }
                found.push(self.variable(name, &entry, base)?);
            }
            // An inlined call or a function is a frame of its own: what is outside is not in scope.
            let scope = self.unit.entry(*offset)?;
            if scope.tag() != DW_TAG_lexical_block {
                break;
            }
        }
        Ok(found)
    }

    fn children_of(&self, offset: UnitOffset) -> Result<Vec<UnitOffset>, ProbeError> {
        let mut tree = self.unit.entries_tree(Some(offset))?;
        let root = tree.root()?;
        let mut children = root.children();
        let mut found = Vec::new();
        while let Some(child) = children.next()? {
            found.push(child.entry().offset());
        }
        Ok(found)
    }

    /// An attribute of an entry, or of what it is a concrete copy of.
    fn attribute(
        &self,
        entry: &Entry<'_>,
        name: gimli::DwAt,
    ) -> Result<Option<AttributeValue<Reader>>, ProbeError> {
        if let Some(value) = entry.attr_value(name) {
            return Ok(Some(value));
        }
        if let Some(AttributeValue::UnitRef(origin)) = entry.attr_value(DW_AT_abstract_origin) {
            let origin = self.unit.entry(origin)?;
            return Ok(origin.attr_value(name));
        }
        Ok(None)
    }

    fn name_of(&self, entry: &Entry<'_>) -> Result<Option<String>, ProbeError> {
        match self.attribute(entry, DW_AT_name)? {
            Some(value) => {
                let name = self.debug.dwarf.attr_string(self.unit, value)?;
                Ok(Some(name.to_string_lossy()?.into_owned()))
            }
            None => Ok(None),
        }
    }

    fn type_of(&self, entry: &Entry<'_>) -> Result<Option<UnitOffset>, ProbeError> {
        Ok(match self.attribute(entry, DW_AT_type)? {
            Some(AttributeValue::UnitRef(offset)) => Some(offset),
            _ => None,
        })
    }

    fn variable(
        &mut self,
        name: String,
        entry: &Entry<'_>,
        base: Option<u32>,
    ) -> Result<Variable, ProbeError> {
        let Some(type_at) = self.type_of(entry)? else {
            return Ok(leaf(name, "?", "<no type>"));
        };
        let Some(place) = self.location(entry, base)? else {
            let type_name = self.type_name(type_at)?;
            return Ok(leaf(name, &type_name, "<optimized out>"));
        };
        self.value(name, type_at, &place, 0)
    }

    /// Where a variable is at this point of the code.
    fn location(&self, entry: &Entry<'_>, base: Option<u32>) -> Result<Option<Place>, ProbeError> {
        let Some(value) = entry.attr_value(DW_AT_location) else {
            return Ok(None);
        };
        if let AttributeValue::Exprloc(expression) = value {
            return self.evaluate(&expression, base);
        }
        let Some(mut locations) = self.debug.dwarf.attr_locations(self.unit, value)? else {
            return Ok(None);
        };
        while let Some(location) = locations.next()? {
            if location.range.begin <= self.pc && self.pc < location.range.end {
                return self.evaluate(&location.data, base);
            }
        }
        Ok(None)
    }

    /// What a location expression comes to. `None` for what it cannot say simply.
    fn evaluate(
        &self,
        expression: &gimli::Expression<Reader>,
        base: Option<u32>,
    ) -> Result<Option<Place>, ProbeError> {
        let mut stack: Vec<u64> = Vec::new();
        let mut held: Option<[u8; 8]> = None;
        let mut value_on_top = false;
        let mut operations = expression.clone().operations(self.unit.encoding());
        while let Some(operation) = operations.next()? {
            match operation {
                Operation::FrameOffset { offset } => {
                    let Some(base) = base else { return Ok(None) };
                    stack.push(u64::from(base).wrapping_add_signed(offset));
                }
                Operation::WasmLocal { index } => {
                    let Some(bytes) = self.frame.local(index) else {
                        return Ok(None);
                    };
                    held = Some(bytes);
                    stack.push(u64::from_le_bytes(bytes));
                    value_on_top = true;
                }
                Operation::WasmGlobal { index } => {
                    let Some(bytes) = self.frame.global(index) else {
                        return Ok(None);
                    };
                    held = Some(bytes);
                    stack.push(u64::from_le_bytes(bytes));
                    value_on_top = true;
                }
                Operation::Address { address } => stack.push(address),
                Operation::UnsignedConstant { value } => stack.push(value),
                Operation::SignedConstant { value } => stack.push(value.cast_unsigned()),
                Operation::PlusConstant { value } => {
                    let Some(top) = stack.pop() else {
                        return Ok(None);
                    };
                    stack.push(top.wrapping_add(value));
                    value_on_top = false;
                    held = None;
                }
                Operation::Deref { .. } => {
                    let Some(address) = stack.pop().and_then(|top| u32::try_from(top).ok()) else {
                        return Ok(None);
                    };
                    let Some(bytes) = self.frame.read(address, 4) else {
                        return Ok(None);
                    };
                    let Ok(bytes) = <[u8; 4]>::try_from(bytes) else {
                        return Ok(None);
                    };
                    stack.push(u64::from(u32::from_le_bytes(bytes)));
                    held = None;
                }
                Operation::StackValue => value_on_top = true,
                _ => return Ok(None),
            }
        }
        let Some(top) = stack.pop() else {
            return Ok(None);
        };
        if value_on_top {
            let bytes = held.unwrap_or(top.to_le_bytes());
            return Ok(Some(Place::Held(bytes.to_vec())));
        }
        Ok(u32::try_from(top).ok().map(Place::Memory))
    }

    fn bytes(&self, place: &Place, len: usize) -> Option<Vec<u8>> {
        match place {
            Place::Memory(address) => self.frame.read(*address, u32::try_from(len).ok()?),
            Place::Held(bytes) => bytes.get(..len).map(<[u8]>::to_vec),
        }
    }

    /// Part of `place`, at `offset`.
    fn at(place: &Place, offset: u64) -> Option<Place> {
        match place {
            Place::Memory(address) => {
                let offset = u32::try_from(offset).ok()?;
                Some(Place::Memory(address.wrapping_add(offset)))
            }
            Place::Held(bytes) => {
                let offset = usize::try_from(offset).ok()?;
                bytes.get(offset..).map(|rest| Place::Held(rest.to_vec()))
            }
        }
    }

    fn type_name(&self, offset: UnitOffset) -> Result<String, ProbeError> {
        let entry = self.unit.entry(offset)?;
        Ok(self.name_of(&entry)?.unwrap_or_else(|| "?".to_string()))
    }

    fn size_of(&self, offset: UnitOffset) -> Result<Option<u64>, ProbeError> {
        let entry = self.unit.entry(offset)?;
        if let Some(size) = entry
            .attr_value(DW_AT_byte_size)
            .and_then(|size| size.udata_value())
        {
            return Ok(Some(size));
        }
        match self.type_of(&entry)? {
            Some(inner) if entry.tag() != DW_TAG_pointer_type => self.size_of(inner),
            _ if entry.tag() == DW_TAG_pointer_type => Ok(Some(4)),
            _ => Ok(None),
        }
    }

    fn value(
        &mut self,
        name: String,
        type_at: UnitOffset,
        place: &Place,
        depth: usize,
    ) -> Result<Variable, ProbeError> {
        let entry = self.unit.entry(type_at)?;
        let type_name = self.name_of(&entry)?.unwrap_or_else(|| "?".to_string());
        self.values += 1;
        if depth > MOST_DEPTH || self.values > MOST_VALUES {
            return Ok(leaf(name, &type_name, "…"));
        }
        let tag = entry.tag();
        if tag == DW_TAG_base_type {
            return Ok(leaf(name, &type_name, &self.base_value(&entry, place)));
        }
        if tag == DW_TAG_typedef
            || tag == DW_TAG_const_type
            || tag == DW_TAG_volatile_type
            || tag == DW_TAG_restrict_type
        {
            return match self.type_of(&entry)? {
                Some(inner) => {
                    let mut variable = self.value(name, inner, place, depth)?;
                    if tag == DW_TAG_typedef {
                        variable.type_name = type_name;
                    }
                    Ok(variable)
                }
                None => Ok(leaf(name, &type_name, "void")),
            };
        }
        if tag == DW_TAG_pointer_type
            || tag == DW_TAG_reference_type
            || tag == DW_TAG_rvalue_reference_type
        {
            return self.pointer(name, &type_name, &entry, place, depth);
        }
        if tag == DW_TAG_enumeration_type {
            return self.c_enum(name, &type_name, &entry, place, type_at);
        }
        if tag == DW_TAG_array_type {
            return self.array(name, &type_name, &entry, place, depth, type_at);
        }
        if tag == DW_TAG_structure_type || tag == DW_TAG_union_type {
            return self.structure(name, &type_name, type_at, place, depth);
        }
        Ok(leaf(name, &type_name, "<unknown kind of type>"))
    }

    fn base_value(&self, entry: &Entry<'_>, place: &Place) -> String {
        let size = entry
            .attr_value(DW_AT_byte_size)
            .and_then(|size| size.udata_value())
            .and_then(|size| usize::try_from(size).ok())
            .unwrap_or(0);
        let encoding = match entry.attr_value(DW_AT_encoding) {
            Some(AttributeValue::Encoding(encoding)) => Some(encoding),
            _ => None,
        };
        let Some(bytes) = self.bytes(place, size) else {
            return "<unreadable>".to_string();
        };
        let mut wide = [0u8; 16];
        let Some(low) = wide.get_mut(..size) else {
            return "<too wide>".to_string();
        };
        low.copy_from_slice(&bytes);
        let unsigned = u128::from_le_bytes(wide);
        if encoding == Some(DW_ATE_boolean) {
            return (unsigned != 0).to_string();
        }
        if encoding == Some(DW_ATE_float) {
            return match size {
                4 => f32::from_le_bytes([wide[0], wide[1], wide[2], wide[3]]).to_string(),
                8 => f64::from_bits(u64::from_le_bytes([
                    wide[0], wide[1], wide[2], wide[3], wide[4], wide[5], wide[6], wide[7],
                ]))
                .to_string(),
                _ => format!("{unsigned:#x}"),
            };
        }
        if encoding == Some(DW_ATE_UTF) {
            return u32::try_from(unsigned)
                .ok()
                .and_then(char::from_u32)
                .map_or_else(|| format!("{unsigned:#x}"), |letter| format!("{letter:?}"));
        }
        if size > 0 && (encoding == Some(DW_ATE_signed) || encoding == Some(DW_ATE_signed_char)) {
            let shift = 128 - size * 8;
            return ((unsigned << shift).cast_signed() >> shift).to_string();
        }
        unsigned.to_string()
    }

    fn pointer(
        &mut self,
        name: String,
        type_name: &str,
        entry: &Entry<'_>,
        place: &Place,
        depth: usize,
    ) -> Result<Variable, ProbeError> {
        let Some(bytes) = self.bytes(place, 4) else {
            return Ok(leaf(name, type_name, "<unreadable>"));
        };
        let Ok(bytes) = <[u8; 4]>::try_from(bytes) else {
            return Ok(leaf(name, type_name, "<unreadable>"));
        };
        let address = u32::from_le_bytes(bytes);
        if address == 0 {
            return Ok(leaf(name, type_name, "null"));
        }
        let Some(pointee) = self.type_of(entry)? else {
            return Ok(leaf(name, type_name, &format!("{address:#x}")));
        };
        let target = self.value("*".to_string(), pointee, &Place::Memory(address), depth + 1)?;
        let value = format!("→ {}", target.value);
        Ok(Variable {
            name,
            type_name: type_name.to_string(),
            value,
            children: if target.children.is_empty() {
                Vec::new()
            } else {
                target.children
            },
        })
    }

    fn c_enum(
        &mut self,
        name: String,
        type_name: &str,
        entry: &Entry<'_>,
        place: &Place,
        offset: UnitOffset,
    ) -> Result<Variable, ProbeError> {
        let size = entry
            .attr_value(DW_AT_byte_size)
            .and_then(|size| size.udata_value())
            .and_then(|size| usize::try_from(size).ok())
            .unwrap_or(4);
        let Some(bytes) = self.bytes(place, size) else {
            return Ok(leaf(name, type_name, "<unreadable>"));
        };
        let mut wide = [0u8; 8];
        if let Some(low) = wide.get_mut(..size) {
            low.copy_from_slice(&bytes);
        }
        let held = u64::from_le_bytes(wide);
        for child in self.children_of(offset)? {
            let enumerator = self.unit.entry(child)?;
            if enumerator.tag() != DW_TAG_enumerator {
                continue;
            }
            let matches = enumerator.attr_value(DW_AT_const_value).and_then(|value| {
                value
                    .udata_value()
                    .or_else(|| value.sdata_value().map(i64::cast_unsigned))
            }) == Some(held);
            if matches {
                let found = self
                    .name_of(&enumerator)?
                    .unwrap_or_else(|| held.to_string());
                return Ok(leaf(name, type_name, &found));
            }
        }
        Ok(leaf(name, type_name, &held.to_string()))
    }

    fn array(
        &mut self,
        name: String,
        type_name: &str,
        entry: &Entry<'_>,
        place: &Place,
        depth: usize,
        offset: UnitOffset,
    ) -> Result<Variable, ProbeError> {
        let Some(item_type) = self.type_of(entry)? else {
            return Ok(leaf(name, type_name, "<no item type>"));
        };
        let mut count = 0u64;
        for child in self.children_of(offset)? {
            let subrange = self.unit.entry(child)?;
            if subrange.tag() != DW_TAG_subrange_type {
                continue;
            }
            if let Some(found) = subrange
                .attr_value(DW_AT_count)
                .and_then(|value| value.udata_value())
            {
                count = found;
            } else if let Some(bound) = subrange
                .attr_value(DW_AT_upper_bound)
                .and_then(|value| value.udata_value())
            {
                count = bound + 1;
            }
            break;
        }
        let item_name = self.type_name(item_type)?;
        let shown_type = if type_name == "?" {
            format!("[{item_name}; {count}]")
        } else {
            type_name.to_string()
        };
        self.items(name, shown_type, item_type, place.clone(), count, depth)
    }

    /// `count` items of `item_type` from `first`.
    fn items(
        &mut self,
        name: String,
        type_name: String,
        item_type: UnitOffset,
        first: Place,
        count: u64,
        depth: usize,
    ) -> Result<Variable, ProbeError> {
        let size = self.size_of(item_type)?.unwrap_or(0);
        let mut children = Vec::new();
        for at in 0..count.min(MOST_ITEMS as u64) {
            let Some(place) = Self::at(&first, at * size) else {
                break;
            };
            children.push(self.value(at.to_string(), item_type, &place, depth + 1)?);
        }
        let more = if count > MOST_ITEMS as u64 {
            format!(", … {count} in all")
        } else {
            String::new()
        };
        let list: Vec<&str> = children.iter().map(|item| item.value.as_str()).collect();
        let value = shorten(format!("[{}{more}]", list.join(", ")));
        Ok(Variable {
            name,
            type_name,
            value,
            children,
        })
    }

    fn text(&self, name: String, type_name: &str, address: u32, len: u64) -> Variable {
        let wanted = u32::try_from(len).unwrap_or(u32::MAX).min(MOST_TEXT);
        let Some(bytes) = self.frame.read(address, wanted) else {
            return leaf(name, type_name, "<unreadable>");
        };
        let more = if u64::from(wanted) < len { "…" } else { "" };
        let text = String::from_utf8_lossy(&bytes);
        leaf(name, type_name, &format!("{text:?}{more}"))
    }

    /// A pointer anywhere in `offset`, the first found going through its fields: where Rust's
    /// collections keep their data, however many wrappers it is under.
    fn first_pointer(
        &self,
        offset: UnitOffset,
        place: &Place,
        depth: usize,
    ) -> Result<Option<u32>, ProbeError> {
        if depth > MOST_DEPTH {
            return Ok(None);
        }
        let entry = self.unit.entry(offset)?;
        if entry.tag() == DW_TAG_pointer_type {
            let bytes = self
                .bytes(place, 4)
                .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok());
            return Ok(bytes.map(u32::from_le_bytes));
        }
        for child in self.children_of(offset)? {
            let member = self.unit.entry(child)?;
            if member.tag() != DW_TAG_member {
                continue;
            }
            let Some(member_type) = self.type_of(&member)? else {
                continue;
            };
            let Some(inner) = Self::at(place, member_offset(&member)) else {
                continue;
            };
            if let Some(found) = self.first_pointer(member_type, &inner, depth + 1)? {
                return Ok(Some(found));
            }
        }
        Ok(None)
    }

    /// A field of `offset`, by name, as its type and where it is.
    fn field(
        &self,
        offset: UnitOffset,
        place: &Place,
        name: &str,
    ) -> Result<Option<(UnitOffset, Place)>, ProbeError> {
        for child in self.children_of(offset)? {
            let member = self.unit.entry(child)?;
            if member.tag() == DW_TAG_member
                && self.name_of(&member)?.as_deref() == Some(name)
                && let Some(member_type) = self.type_of(&member)?
                && let Some(inner) = Self::at(place, member_offset(&member))
            {
                return Ok(Some((member_type, inner)));
            }
        }
        Ok(None)
    }

    /// An unsigned field of `offset`, by name.
    fn field_u32(
        &self,
        offset: UnitOffset,
        place: &Place,
        name: &str,
    ) -> Result<Option<u32>, ProbeError> {
        let Some((_, inner)) = self.field(offset, place, name)? else {
            return Ok(None);
        };
        let bytes = self
            .bytes(&inner, 4)
            .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok());
        Ok(bytes.map(u32::from_le_bytes))
    }

    fn template_type(&self, offset: UnitOffset) -> Result<Option<UnitOffset>, ProbeError> {
        for child in self.children_of(offset)? {
            let parameter = self.unit.entry(child)?;
            if parameter.tag() == DW_TAG_template_type_parameter
                && self.name_of(&parameter)?.as_deref() == Some("T")
            {
                return self.type_of(&parameter);
            }
        }
        Ok(None)
    }

    /// Rust's text and lists, as what they hold: `None` for any other type, or one laid out in
    /// a way this does not know, which is then shown field by field.
    fn rust_collection(
        &mut self,
        name: &str,
        type_name: &str,
        offset: UnitOffset,
        place: &Place,
        depth: usize,
    ) -> Result<Option<Variable>, ProbeError> {
        // `&str` and `&[T]`: a pointer and a length.
        if type_name == "&str" || type_name.starts_with("&[") || type_name.starts_with("&mut [") {
            let Some(pointer) = self.first_pointer(offset, place, 0)? else {
                return Ok(None);
            };
            let Some(length) = self.field_u32(offset, place, "length")? else {
                return Ok(None);
            };
            if type_name == "&str" {
                return Ok(Some(self.text(
                    name.to_string(),
                    type_name,
                    pointer,
                    u64::from(length),
                )));
            }
            let Some((data, _)) = self.field(offset, place, "data_ptr")? else {
                return Ok(None);
            };
            let data = self.unit.entry(data)?;
            let Some(item_type) = self.type_of(&data)? else {
                return Ok(None);
            };
            return self
                .items(
                    name.to_string(),
                    type_name.to_string(),
                    item_type,
                    Place::Memory(pointer),
                    u64::from(length),
                    depth,
                )
                .map(Some);
        }
        if type_name == "String" {
            let Some((vec_type, vec_place)) = self.field(offset, place, "vec")? else {
                return Ok(None);
            };
            let Some(pointer) = self.first_pointer(vec_type, &vec_place, 0)? else {
                return Ok(None);
            };
            let Some(length) = self.field_u32(vec_type, &vec_place, "len")? else {
                return Ok(None);
            };
            return Ok(Some(self.text(
                name.to_string(),
                type_name,
                pointer,
                u64::from(length),
            )));
        }
        if type_name.starts_with("Vec<") {
            let Some(item_type) = self.template_type(offset)? else {
                return Ok(None);
            };
            let Some(pointer) = self.first_pointer(offset, place, 0)? else {
                return Ok(None);
            };
            let Some(length) = self.field_u32(offset, place, "len")? else {
                return Ok(None);
            };
            return self
                .items(
                    name.to_string(),
                    type_name.to_string(),
                    item_type,
                    Place::Memory(pointer),
                    u64::from(length),
                    depth,
                )
                .map(Some);
        }
        Ok(None)
    }

    fn structure(
        &mut self,
        name: String,
        type_name: &str,
        offset: UnitOffset,
        place: &Place,
        depth: usize,
    ) -> Result<Variable, ProbeError> {
        if let Some(collection) = self.rust_collection(&name, type_name, offset, place, depth)? {
            return Ok(collection);
        }
        let mut members = Vec::new();
        let mut variant = None;
        for child in self.children_of(offset)? {
            let entry = self.unit.entry(child)?;
            if entry.tag() == DW_TAG_member {
                members.push(child);
            } else if entry.tag() == DW_TAG_variant_part {
                variant = Some(child);
            }
        }
        if let Some(part) = variant {
            return self.rust_enum(name, type_name, part, place, depth);
        }
        let mut children = Vec::new();
        for member in members {
            let entry = self.unit.entry(member)?;
            let Some(member_type) = self.type_of(&entry)? else {
                continue;
            };
            let Some(field) = self.name_of(&entry)? else {
                continue;
            };
            let Some(inner) = Self::at(place, member_offset(&entry)) else {
                continue;
            };
            // What only marks a type takes no room, and says nothing.
            if self.size_of(member_type)? == Some(0) {
                continue;
            }
            children.push(self.value(field, member_type, &inner, depth + 1)?);
        }
        let value = summary_of(type_name, &children);
        Ok(Variable {
            name,
            type_name: type_name.to_string(),
            value,
            children,
        })
    }

    /// A Rust enum: its discriminant says which variant, whose fields then follow.
    fn rust_enum(
        &mut self,
        name: String,
        type_name: &str,
        part: UnitOffset,
        place: &Place,
        depth: usize,
    ) -> Result<Variable, ProbeError> {
        let part_entry = self.unit.entry(part)?;
        let held = match part_entry.attr_value(DW_AT_discr) {
            Some(AttributeValue::UnitRef(discriminant)) => {
                let member = self.unit.entry(discriminant)?;
                let size = self
                    .type_of(&member)?
                    .map(|inner| self.size_of(inner))
                    .transpose()?
                    .flatten()
                    .and_then(|size| usize::try_from(size).ok())
                    .unwrap_or(4);
                Self::at(place, member_offset(&member))
                    .and_then(|inner| self.bytes(&inner, size))
                    .map(|bytes| {
                        let mut wide = [0u8; 8];
                        if let Some(low) = wide.get_mut(..size) {
                            low.copy_from_slice(&bytes);
                        }
                        u64::from_le_bytes(wide)
                    })
            }
            _ => None,
        };
        let mut default = None;
        let mut chosen = None;
        for child in self.children_of(part)? {
            let entry = self.unit.entry(child)?;
            if entry.tag() != DW_TAG_variant {
                continue;
            }
            match entry
                .attr_value(DW_AT_discr_value)
                .and_then(|value| value.udata_value())
            {
                Some(value) if Some(value) == held => chosen = Some(child),
                None => default = Some(child),
                _ => {}
            }
        }
        let Some(variant) = chosen.or(default) else {
            return Ok(leaf(name, type_name, "<unknown variant>"));
        };
        for child in self.children_of(variant)? {
            let member = self.unit.entry(child)?;
            if member.tag() != DW_TAG_member {
                continue;
            }
            let label = self.name_of(&member)?.unwrap_or_default();
            let Some(fields_type) = self.type_of(&member)? else {
                continue;
            };
            let Some(inner) = Self::at(place, member_offset(&member)) else {
                continue;
            };
            let fields = self.structure(label.clone(), &label, fields_type, &inner, depth + 1)?;
            return Ok(Variable {
                name,
                type_name: type_name.to_string(),
                value: fields.value,
                children: fields.children,
            });
        }
        Ok(leaf(name, type_name, "<unknown variant>"))
    }
}

fn member_offset(member: &Entry<'_>) -> u64 {
    member
        .attr_value(DW_AT_data_member_location)
        .and_then(|value| value.udata_value())
        .unwrap_or(0)
}

fn leaf(name: String, type_name: &str, value: &str) -> Variable {
    Variable {
        name,
        type_name: type_name.to_string(),
        value: value.to_string(),
        children: Vec::new(),
    }
}

fn shorten(text: String) -> String {
    if text.chars().count() <= MOST_SUMMARY {
        return text;
    }
    let cut: String = text.chars().take(MOST_SUMMARY).collect();
    format!("{cut}…")
}

/// A struct on one line, as its source would write it. Tuples and tuple variants have fields
/// named `__0`, `__1` and so on.
fn summary_of(type_name: &str, children: &[Variable]) -> String {
    let short = type_name.rsplit("::").next().unwrap_or(type_name);
    if children.is_empty() {
        return if type_name == "?" {
            "{}".to_string()
        } else {
            short.to_string()
        };
    }
    let tuple = children.iter().all(|child| child.name.starts_with("__"));
    let fields: Vec<String> = children
        .iter()
        .map(|child| {
            if tuple {
                child.value.clone()
            } else {
                format!("{}: {}", child.name, child.value)
            }
        })
        .collect();
    let text = if type_name.starts_with('(') {
        format!("({})", fields.join(", "))
    } else if tuple {
        format!("{short}({})", fields.join(", "))
    } else {
        format!("{short} {{ {} }}", fields.join(", "))
    };
    shorten(text)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::{Lines, core_modules, instrument};

    const SOURCE: &str =
        include_str!("../../../test-fixtures/test-components/guests/line-walk/src/lib.rs");

    fn marked(name: &str) -> u32 {
        let wanted = format!("// mark: {name}");
        let at = SOURCE
            .lines()
            .position(|line| line.trim_end().ends_with(&wanted))
            .unwrap_or_else(|| panic!("no mark `{name}`"));
        u32::try_from(at).unwrap() + 1
    }

    /// A frame with nothing to read: scopes and types do not need one.
    struct Empty;
    impl Frame for Empty {
        fn local(&self, _: u32) -> Option<[u8; 8]> {
            Some([0; 8])
        }
        fn global(&self, _: u32) -> Option<[u8; 8]> {
            Some([0; 8])
        }
        fn read(&self, _: u32, _: u32) -> Option<Vec<u8>> {
            None
        }
    }

    fn names_at(mark: &str) -> Vec<String> {
        let modules = core_modules(test_components::LINE_WALK.wasm).unwrap();
        let module = modules.iter().max_by_key(|module| module.len()).unwrap();
        let instrumented = instrument(module, Lines::Own).unwrap().unwrap();
        let line = marked(mark);
        let probe = instrumented
            .probes
            .iter()
            .find(|probe| probe.line == line)
            .unwrap();
        let debug = ModuleDebug::new(module).unwrap().unwrap();
        debug
            .variables_at(probe.offset, &Empty)
            .unwrap()
            .into_iter()
            .map(|variable| variable.name)
            .collect()
    }

    #[test]
    fn a_let_is_in_scope_only_after_it() {
        let first = names_at("walk-first");
        assert!(first.contains(&"x".to_string()), "{first:?}");
        assert!(!first.contains(&"a".to_string()), "{first:?}");
        let second = names_at("walk-second");
        assert!(second.contains(&"a".to_string()), "{second:?}");
        assert!(second.contains(&"x".to_string()), "{second:?}");
        assert!(!second.contains(&"b".to_string()), "{second:?}");
    }

    #[test]
    fn scopes_end_at_the_function() {
        let body = names_at("add-body");
        assert_eq!(
            {
                let mut sorted = body.clone();
                sorted.sort();
                sorted
            },
            ["a", "b"],
            "not `x`, its caller's",
        );
    }

    #[test]
    fn every_kind_is_in_scope_at_the_end() {
        let names = names_at("kinds-end");
        for name in ["seed", "count", "text", "owned", "list", "rect", "boxed"] {
            assert!(
                names.iter().any(|found| found == name),
                "{name} in {names:?}"
            );
        }
    }

    #[test]
    fn a_module_with_no_debug_info_has_nothing_to_read() {
        let module = wat::parse_str("(module (func))").unwrap();
        assert!(ModuleDebug::new(&module).unwrap().is_none());
    }

    #[test]
    fn summaries_read_as_the_source_would_write_them() {
        let field = |name: &str, value: &str| leaf(name.to_string(), "u32", value);
        assert_eq!(
            summary_of(
                "line_walk_guest::Point",
                &[field("x", "3"), field("y", "2.5")]
            ),
            "Point { x: 3, y: 2.5 }",
        );
        assert_eq!(summary_of("Some", &[field("__0", "7")]), "Some(7)");
        assert_eq!(summary_of("None", &[]), "None");
        assert_eq!(
            summary_of("(u8, u16)", &[field("__0", "1"), field("__1", "2")]),
            "(1, 2)"
        );
    }
}
