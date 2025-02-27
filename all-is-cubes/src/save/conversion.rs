//! Conversion between the types in [`super::schema`] and those used in
//! normal operation.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::schema;

/// Implements [`Serialize`] and [`Deserialize`] for `$library_type` using the conversions
/// * `TryFrom<$schema_type> for $library_type`
/// * `From<&$library_type> for $schema_type`
#[allow(unused)] // TODO: use this
macro_rules! impl_serde_via_schema_by_ref {
    ($library_type:ty, $schema_type:ty) => {
        impl ::serde::Serialize for $library_type {
            fn serialize<S: ::serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                // Construct wrapper by reference, unlike #[serde(into)]
                let schema_form: $schema_type = <$schema_type as From<&$library_type>>::from(self);
                <$schema_type as ::serde::Serialize>::serialize(&schema_form, serializer)
            }
        }
        impl<'de> ::serde::Deserialize<'de> for $library_type {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: ::serde::Deserializer<'de>,
            {
                // This is basically `#[serde(try_from = $schema_type)]`.

                let schema_form: $schema_type =
                    <$schema_type as ::serde::Deserialize<'de>>::deserialize(deserializer)?;
                // TODO: Don't convert error here
                <$library_type as std::convert::TryFrom<$schema_type>>::try_from(schema_form)
                    .map_err(serde::de::Error::custom)
            }
        }
    };
}

mod block {
    use super::*;
    use crate::block::{Block, BlockAttributes, Composite, Modifier, Move, Primitive, Quote, Zoom};
    use crate::math::Rgba;
    use schema::{BlockSer, ModifierSer};

    impl Serialize for Block {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            BlockSer::BlockV1 {
                primitive: schema::PrimitiveSer::from(self.primitive()),
                modifiers: self
                    .modifiers()
                    .iter()
                    .map(schema::ModifierSer::from)
                    .collect(),
            }
            .serialize(serializer)
        }
    }

    impl<'de> Deserialize<'de> for Block {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            Ok(match BlockSer::deserialize(deserializer)? {
                BlockSer::BlockV1 {
                    primitive,
                    modifiers,
                } => {
                    let mut block = Block::from_primitive(primitive.into());
                    block
                        .modifiers_mut()
                        .extend(modifiers.into_iter().map(Modifier::from));
                    block
                }
            })
        }
    }

    impl From<&Primitive> for schema::PrimitiveSer {
        fn from(value: &Primitive) -> Self {
            match value {
                Primitive::Indirect(definition) => schema::PrimitiveSer::IndirectV1 {
                    definition: definition.clone(),
                },
                &Primitive::Atom(ref attributes, color) => schema::PrimitiveSer::AtomV1 {
                    color: color.into(),
                    attributes: attributes.into(),
                },
                &Primitive::Recur {
                    ref attributes,
                    ref space,
                    offset,
                    resolution,
                } => schema::PrimitiveSer::RecurV1 {
                    attributes: attributes.into(),
                    space: space.clone(),
                    offset: offset.into(),
                    resolution,
                },
                Primitive::Air => schema::PrimitiveSer::AirV1,
            }
        }
    }

    impl From<schema::PrimitiveSer> for Primitive {
        fn from(value: schema::PrimitiveSer) -> Self {
            match value {
                schema::PrimitiveSer::IndirectV1 { definition } => Primitive::Indirect(definition),
                schema::PrimitiveSer::AtomV1 { attributes, color } => {
                    Primitive::Atom(BlockAttributes::from(attributes), Rgba::from(color))
                }
                schema::PrimitiveSer::RecurV1 {
                    attributes,
                    space,
                    offset,
                    resolution,
                } => Primitive::Recur {
                    attributes: attributes.into(),
                    space,
                    offset: offset.into(),
                    resolution,
                },
                schema::PrimitiveSer::AirV1 => Primitive::Air,
            }
        }
    }

    impl From<&BlockAttributes> for schema::BlockAttributesV1Ser {
        fn from(value: &BlockAttributes) -> Self {
            let &BlockAttributes {
                // TODO: implement serializing all attributes
                ref display_name,
                selectable,
                collision: _,
                rotation_rule: _,
                light_emission,
                tick_action: _,
                animation_hint: _,
            } = value;
            schema::BlockAttributesV1Ser {
                display_name: display_name.to_string(),
                selectable,
                light_emission: light_emission.into(),
            }
        }
    }

    impl From<schema::BlockAttributesV1Ser> for BlockAttributes {
        fn from(value: schema::BlockAttributesV1Ser) -> Self {
            // TODO: implement deserializing all attributes
            let schema::BlockAttributesV1Ser {
                display_name,
                selectable,
                light_emission,
            } = value;
            Self {
                display_name: display_name.into(),
                selectable,
                light_emission: light_emission.into(),
                ..Default::default()
            }
        }
    }

    impl From<&Modifier> for ModifierSer {
        fn from(value: &Modifier) -> Self {
            match *value {
                Modifier::Quote(Quote { suppress_ambient }) => {
                    ModifierSer::QuoteV1 { suppress_ambient }
                }
                Modifier::Rotate(rotation) => ModifierSer::RotateV1 { rotation },
                Modifier::Composite(Composite {
                    ref source,
                    operator,
                    reverse,
                    disassemblable,
                }) => ModifierSer::CompositeV1 {
                    source: source.clone(),
                    operator,
                    reverse,
                    disassemblable,
                },
                Modifier::Zoom(ref m) => m.to_serial_schema(),
                Modifier::Move(Move {
                    direction,
                    distance,
                    velocity,
                }) => ModifierSer::MoveV1 {
                    direction,
                    distance,
                    velocity,
                },
            }
        }
    }

    impl From<schema::ModifierSer> for Modifier {
        fn from(value: schema::ModifierSer) -> Self {
            match value {
                ModifierSer::QuoteV1 { suppress_ambient } => {
                    Modifier::Quote(Quote { suppress_ambient })
                }
                ModifierSer::RotateV1 { rotation } => Modifier::Rotate(rotation),
                ModifierSer::CompositeV1 {
                    source,
                    operator,
                    reverse,
                    disassemblable,
                } => Modifier::Composite(Composite {
                    source,
                    operator,
                    reverse,
                    disassemblable,
                }),
                ModifierSer::ZoomV1 { scale, offset } => {
                    Modifier::Zoom(Zoom::new(scale, offset.map(i32::from).into()))
                }
                ModifierSer::MoveV1 {
                    direction,
                    distance,
                    velocity,
                } => Modifier::Move(Move::new(direction, distance, velocity)),
            }
        }
    }
}

// `character::Character` serialization is inside its module for the sake of private fields.

mod math {
    use super::*;
    use crate::math::{Aab, GridAab};

    impl Serialize for Aab {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            schema::AabSer {
                lower: self.lower_bounds_p().into(),
                upper: self.upper_bounds_p().into(),
            }
            .serialize(serializer)
        }
    }

    impl<'de> Deserialize<'de> for Aab {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: Deserializer<'de>,
        {
            let schema::AabSer { lower, upper } = schema::AabSer::deserialize(deserializer)?;
            Aab::checked_from_lower_upper(lower.into(), upper.into())
                .ok_or_else(|| serde::de::Error::custom("invalid AAB"))
        }
    }

    impl Serialize for GridAab {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            schema::GridAabSer {
                lower: self.lower_bounds().into(),
                upper: self.upper_bounds().into(),
            }
            .serialize(serializer)
        }
    }

    impl<'de> Deserialize<'de> for GridAab {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: Deserializer<'de>,
        {
            let schema::GridAabSer { lower, upper } =
                schema::GridAabSer::deserialize(deserializer)?;
            GridAab::checked_from_lower_upper(lower, upper).map_err(serde::de::Error::custom)
        }
    }
}

mod inv {
    use super::*;
    use crate::inv::{EphemeralOpaque, Inventory, Slot, Tool};

    impl Serialize for Inventory {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            schema::InventorySer::InventoryV1 {
                slots: self
                    .slots
                    .iter()
                    .map(|slot| match *slot {
                        crate::inv::Slot::Empty => None,
                        crate::inv::Slot::Stack(count, ref item) => Some(schema::InvStackSer {
                            count,
                            item: item.clone(),
                        }),
                    })
                    .collect(),
            }
            .serialize(serializer)
        }
    }

    impl<'de> Deserialize<'de> for Inventory {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: Deserializer<'de>,
        {
            match schema::InventorySer::deserialize(deserializer)? {
                schema::InventorySer::InventoryV1 { slots } => Ok(Inventory {
                    slots: slots
                        .into_iter()
                        .map(|slot| match slot {
                            Some(schema::InvStackSer { count, item }) => Slot::Stack(count, item),
                            None => Slot::Empty,
                        })
                        .collect(),
                }),
            }
        }
    }

    impl Serialize for Tool {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            match *self {
                Tool::Activate => schema::ToolSer::ActivateV1 {},
                Tool::RemoveBlock { keep } => schema::ToolSer::RemoveBlockV1 { keep },
                Tool::Block(ref block) => schema::ToolSer::BlockV1 {
                    block: block.clone(),
                },
                Tool::InfiniteBlocks(ref block) => schema::ToolSer::InfiniteBlocksV1 {
                    block: block.clone(),
                },
                Tool::CopyFromSpace => schema::ToolSer::CopyFromSpaceV1 {},
                Tool::EditBlock => schema::ToolSer::EditBlockV1 {},
                Tool::PushPull => schema::ToolSer::PushPullV1 {},
                Tool::Jetpack { active } => schema::ToolSer::JetpackV1 { active },
                Tool::ExternalAction {
                    function: _,
                    ref icon,
                } => schema::ToolSer::ExternalActionV1 { icon: icon.clone() },
            }
            .serialize(serializer)
        }
    }

    impl<'de> Deserialize<'de> for Tool {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: Deserializer<'de>,
        {
            Ok(match schema::ToolSer::deserialize(deserializer)? {
                schema::ToolSer::ActivateV1 {} => Tool::Activate,
                schema::ToolSer::RemoveBlockV1 { keep } => Tool::RemoveBlock { keep },
                schema::ToolSer::BlockV1 { block } => Tool::Block(block),
                schema::ToolSer::InfiniteBlocksV1 { block } => Tool::InfiniteBlocks(block),
                schema::ToolSer::CopyFromSpaceV1 {} => Tool::CopyFromSpace,
                schema::ToolSer::EditBlockV1 {} => Tool::EditBlock,
                schema::ToolSer::PushPullV1 {} => Tool::PushPull,
                schema::ToolSer::JetpackV1 { active } => Tool::Jetpack { active },
                schema::ToolSer::ExternalActionV1 { icon } => Tool::ExternalAction {
                    function: EphemeralOpaque(None),
                    icon,
                },
            })
        }
    }
}

mod space {
    use super::*;
    use crate::space::Space;

    impl Serialize for Space {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            // TODO: more efficient serialization without extract() and with some kind of compression
            schema::SpaceSer::SpaceV1 {
                bounds: self.bounds(),
                blocks: self
                    .block_data()
                    .iter()
                    .map(|bd| bd.block().clone())
                    .collect(),
                contents: self
                    .extract(self.bounds(), |index, _, _| {
                        index.expect("shouldn't happen: serialization went out of bounds")
                    })
                    .into_elements(),
            }
            .serialize(serializer)
        }
    }

    impl<'de> Deserialize<'de> for Space {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: Deserializer<'de>,
        {
            match schema::SpaceSer::deserialize(deserializer)? {
                schema::SpaceSer::SpaceV1 {
                    bounds,
                    blocks,
                    contents,
                } => {
                    // TODO: more efficient loading that sets blocks by index rather than value
                    let mut space = Space::builder(bounds).build();
                    for (cube, &block_index) in bounds.interior_iter().zip(contents.iter()) {
                        space
                            .set(
                                cube,
                                blocks.get(usize::from(block_index)).ok_or_else(|| {
                                    serde::de::Error::custom(format!(
                                    "Space contents block index {block_index} out of bounds of \
                                    block table length {len}",
                                    len = blocks.len()
                                ))
                                })?,
                            )
                            .unwrap();
                    }
                    Ok(space)
                }
            }
        }
    }
}

mod universe {
    use super::*;
    use crate::block::{Block, BlockDef};
    use crate::character::Character;
    use crate::save::schema::MemberEntrySer;
    use crate::space::Space;
    use crate::universe::{Name, PartialUniverse, UBorrow, URef, Universe};
    use schema::{MemberDe, NameSer, URefSer};

    impl From<&BlockDef> for schema::MemberSer {
        fn from(block_def: &BlockDef) -> Self {
            let block: &Block = block_def;
            schema::MemberSer::BlockDef(block.clone())
        }
    }

    impl Serialize for PartialUniverse {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            let Self {
                blocks,
                characters,
                spaces,
            } = self;

            let blocks = blocks.iter().map(|member_ref: &URef<BlockDef>| {
                let name = member_ref.name();
                let read_guard: UBorrow<BlockDef> = member_ref.read().map_err(|e| {
                    serde::ser::Error::custom(format!("Failed to read universe member {name}: {e}"))
                })?;
                let member_repr = schema::MemberSer::from(&*read_guard);
                Ok(schema::MemberEntrySer {
                    name: member_ref.name(),
                    value: member_repr,
                })
            });
            let characters = characters.iter().map(|member_ref: &URef<Character>| {
                Ok(schema::MemberEntrySer {
                    name: member_ref.name(),
                    value: schema::MemberSer::Character(schema::SerializeRef(member_ref.clone())),
                })
            });
            let spaces = spaces.iter().map(|member_ref: &URef<Space>| {
                Ok(schema::MemberEntrySer {
                    name: member_ref.name(),
                    value: schema::MemberSer::Space(schema::SerializeRef(member_ref.clone())),
                })
            });

            schema::UniverseSer::UniverseV1 {
                members: blocks
                    .chain(characters)
                    .chain(spaces)
                    .collect::<Result<Vec<MemberEntrySer<schema::MemberSer>>, S::Error>>()?,
            }
            .serialize(serializer)
        }
    }

    impl Serialize for Universe {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            PartialUniverse::all_of(self).serialize(serializer)
        }
    }

    impl<'de> Deserialize<'de> for Universe {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            let data = schema::UniverseDe::deserialize(deserializer)?;
            let mut universe = Universe::new();
            match data {
                schema::UniverseDe::UniverseV1 { members } => {
                    for schema::MemberEntrySer { name, value } in members {
                        match value {
                            MemberDe::BlockDef(block) => {
                                universe.insert(name, BlockDef::new(block)).map(|_| ())
                            }
                            MemberDe::Character(character) => {
                                universe.insert(name, character).map(|_| ())
                            }
                            MemberDe::Space(space) => universe.insert(name, space).map(|_| ()),
                        }
                        .expect("insertion from deserialization failed");
                    }
                }
            }
            Ok(universe)
        }
    }

    impl<T: 'static> Serialize for URef<T> {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            URefSer::URefV1 { name: self.name() }.serialize(serializer)
        }
    }

    impl<'de, T: 'static> Deserialize<'de> for URef<T> {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            Ok(match URefSer::deserialize(deserializer)? {
                // TODO: Instead of new_gone(), this needs to be a named ref that can be
                // hooked up to its definition.
                URefSer::URefV1 { name } => URef::new_gone(name),
            })
        }
    }

    impl Serialize for Name {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            match self {
                Name::Specific(s) => NameSer::Specific(s.clone()),
                &Name::Anonym(number) => NameSer::Anonym(number),
                Name::Pending => {
                    return Err(serde::ser::Error::custom("cannot serialize a pending URef"))
                }
            }
            .serialize(serializer)
        }
    }

    impl<'de> Deserialize<'de> for Name {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            Ok(match NameSer::deserialize(deserializer)? {
                NameSer::Specific(s) => Name::Specific(s),
                NameSer::Anonym(n) => Name::Anonym(n),
            })
        }
    }

    impl<T: Serialize + 'static> Serialize for schema::SerializeRef<T> {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            let uref: &URef<T> = &self.0;
            let read_guard: UBorrow<T> = uref.read().map_err(|e| {
                serde::ser::Error::custom(format!(
                    "Failed to read universe member {name}: {e}",
                    name = uref.name()
                ))
            })?;
            let value: &T = &read_guard;
            value.serialize(serializer)
        }
    }
}
