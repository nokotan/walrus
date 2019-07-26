//! Data segments within a wasm module.

use crate::emit::{Emit, EmitContext, Section};
use crate::ir::Value;
use crate::parse::IndicesToIds;
use crate::tombstone_arena::{Id, Tombstone, TombstoneArena};
use crate::{GlobalId, InitExpr, MemoryId, Module, Result, ValType};
use failure::{bail, ResultExt};

/// A passive element segment identifier
pub type DataId = Id<Data>;

/// A data segment.
///
/// Every data segment has an associated value. This value gets copied into a
/// memory. It is either automatically copied into a specific memory at Wasm
/// instantiation time (active data segments) or dynamically copied into a
/// memory (or memories) via the `memory.init` instruction (passive data
/// segments). See the `kind` member and `DataKind` type for more details on the
/// active/passive distinction.
#[derive(Debug)]
pub struct Data {
    id: DataId,
    /// What kind of data segment is this? Passive or active?
    pub kind: DataKind,
    /// The data payload of this data segment.
    pub value: Vec<u8>,
}

/// The kind of data segment: passive or active.
#[derive(Debug)]
pub enum DataKind {
    /// An active data segment that is automatically initialized at some address
    /// in a static memory.
    Active(ActiveData),
    /// A passive data segment that must be manually initialized at a dynamic
    /// address via the `memory.init` instruction (perhaps multiple times in
    /// multiple different memories) and then manually freed when it's no longer
    /// needed via the `data.drop` instruction.
    Passive,
}

/// The parts of a data segment that are only present in active data segments.
#[derive(Clone, Debug)]
pub struct ActiveData {
    /// The memory that this active data segment will be automatically
    /// initialized in.
    pub memory: MemoryId,
    /// The memory location where this active data segment will be automatically
    /// initialized.
    pub location: ActiveDataLocation,
}

/// The memory location where an active data segment will be automatically
/// initialized.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActiveDataLocation {
    /// A static, absolute address within the memory.
    Absolute(u32),
    /// A relative address (expressed as a global's value) within the memory.
    Relative(GlobalId),
}

impl Tombstone for Data {
    fn on_delete(&mut self) {
        self.value = Vec::new();
    }
}

impl Data {
    /// Returns the id of this passive data segment
    pub fn id(&self) -> DataId {
        self.id
    }

    /// Is this a passive data segment?
    pub fn is_passive(&self) -> bool {
        match self.kind {
            DataKind::Passive => true,
            _ => false,
        }
    }
}

/// All passive data sections of a wasm module, used to initialize memories via
/// various instructions.
#[derive(Debug, Default)]
pub struct ModuleData {
    arena: TombstoneArena<Data>,
}

impl ModuleData {
    /// Get an element associated with an ID
    pub fn get(&self, id: DataId) -> &Data {
        &self.arena[id]
    }

    /// Get an element associated with an ID
    pub fn get_mut(&mut self, id: DataId) -> &mut Data {
        &mut self.arena[id]
    }

    /// Delete a passive data segment from this module.
    ///
    /// It is up to you to ensure that all references to the deleted segment are
    /// removed, eg `memory.init` and `data.drop` expressions.
    pub fn delete(&mut self, id: DataId) {
        self.arena.delete(id);
    }

    /// Get a shared reference to this module's passive elements.
    pub fn iter(&self) -> impl Iterator<Item = &Data> {
        self.arena.iter().map(|(_, f)| f)
    }

    // Note that this is inaccordance with the upstream bulk memory proposal to
    // WebAssembly and isn't currently part of the WebAssembly standard.
    pub(crate) fn emit_data_count(&self, cx: &mut EmitContext) {
        let mut count = 0;

        for data in self.iter() {
            cx.indices.set_data_index(data.id(), count as u32);
            count += 1;
        }

        // Technically, we /could/ emit slightly smaller binaries by only adding
        // this section when either:
        //
        // * there are passive data segments, or
        //
        // * there are `memory.init` or `data.drop` instructions (99.99% overlap
        //   with the previous case, but you technically can try and drop active
        //   data segments and get a runtime error!)
        //
        // However, checking that second case can be somewhat expensive (iterate
        // over every instruction in every function), so we just always emit the
        // `DataCount` section when we have data segments. At most this is five
        // unnecessary bytes, so not a big deal in the grand scheme of things.
        cx.start_section(Section::DataCount).encoder.usize(count);
    }
}

impl Module {
    /// Called when we see the data section section to create an id for all data
    /// indices
    ///
    /// Note that during function parsing all data indices less than `count` are
    /// considered valid, and it's only afterwards that we discover whether
    /// they're actually passive or not, and that property is checked during
    /// validation.
    pub(crate) fn reserve_data(&mut self, count: u32, ids: &mut IndicesToIds) {
        log::debug!("reserving space for {} data segments", count);
        for _ in 0..count {
            ids.push_data(self.data.arena.alloc_with_id(|id| Data {
                id,
                // NB: We'll update the `value` and `kind` once we actually
                // parse the data segments.
                value: Vec::new(),
                kind: DataKind::Passive,
            }));
        }
    }

    /// Parses a raw wasm section into a fully-formed `ModuleData` instance.
    pub(crate) fn parse_data(
        &mut self,
        section: wasmparser::DataSectionReader,
        ids: &IndicesToIds,
        data_count: Option<u32>,
    ) -> Result<()> {
        log::debug!("parse data section");
        if let Some(count) = data_count {
            if count != section.get_count() {
                bail!("data count section mismatches actual data section");
            }
        }
        for (i, segment) in section.into_iter().enumerate() {
            let segment = segment?;

            // If we had the `DataCount` section, then we already pre-allocated
            // a data segment. Otherwise, allocate one now.
            let id = if data_count.is_some() {
                ids.get_data(i as u32)?
            } else {
                self.data.arena.alloc_with_id(|id| Data {
                    id,
                    value: Vec::new(),
                    kind: DataKind::Passive,
                })
            };
            let data = self.data.get_mut(id);

            match segment.kind {
                wasmparser::DataKind::Passive => {
                    data.value = segment.data.to_vec();
                    data.kind = DataKind::Passive;
                }
                wasmparser::DataKind::Active {
                    memory_index,
                    init_expr,
                } => {
                    data.value = segment.data.to_vec();

                    let memory_id = ids.get_memory(memory_index)?;
                    let memory = self.memories.get_mut(memory_id);
                    memory.data_segments.insert(data.id);

                    let offset = InitExpr::eval(&init_expr, ids)
                        .with_context(|_e| format!("in segment {}", i))?;
                    data.kind = DataKind::Active(ActiveData {
                        memory: memory_id,
                        location: match offset {
                            InitExpr::Value(Value::I32(n)) => {
                                ActiveDataLocation::Absolute(n as u32)
                            }
                            InitExpr::Global(global)
                                if self.globals.get(global).ty == ValType::I32 =>
                            {
                                ActiveDataLocation::Relative(global)
                            }
                            _ => bail!("non-i32 constant in segment {}", i),
                        },
                    });
                }
            }
        }
        Ok(())
    }
}

impl Emit for ModuleData {
    fn emit(&self, cx: &mut EmitContext) {
        log::debug!("emit data section");
        if self.arena.len() == 0 {
            return;
        }

        let mut cx = cx.start_section(Section::Data);
        cx.encoder.usize(self.arena.len());

        // The encodings here are with respect to the bulk memory proposal, but
        // should be backwards compatible with the current MVP WebAssembly spec
        // so long as the only memory 0 is used.
        for data in self.iter() {
            match data.kind {
                DataKind::Passive => {
                    cx.encoder.byte(0x01);
                    cx.encoder.bytes(&data.value);
                }
                DataKind::Active(ref a) => {
                    let index = cx.indices.get_memory_index(a.memory);
                    if index == 0 {
                        cx.encoder.byte(0x00);
                    } else {
                        cx.encoder.byte(0x02);
                        cx.encoder.u32(index);
                    }
                    let init_expr = match a.location {
                        ActiveDataLocation::Absolute(a) => InitExpr::Value(Value::I32(a as i32)),
                        ActiveDataLocation::Relative(g) => InitExpr::Global(g),
                    };
                    init_expr.emit(&mut cx);
                    cx.encoder.bytes(&data.value);
                }
            }
        }
    }
}
