/// Encumbrance class for carrying a resource in hands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bulk {
    /// Many fit per hand (food, seeds, cloth, tools).
    Small,
    /// One stack per hand (single weapons, armor pieces, coal).
    OneHand,
    /// Both hands required for one stack (logs, stone blocks, iron ingots).
    TwoHand,
}

impl Bulk {
    /// Catalog-driven bulk lookup. Returns `None` only when the resource
    /// is unknown to the catalog (which indicates a programming error:
    /// the catalog must define every resource referenced by simulation
    /// code).
    pub fn for_resource(
        id: super::resource_catalog::ResourceId,
        catalog: &super::resource_catalog::ResourceCatalog,
    ) -> Option<Bulk> {
        catalog.get(id).map(|d| d.bulk.as_bulk())
    }
}
