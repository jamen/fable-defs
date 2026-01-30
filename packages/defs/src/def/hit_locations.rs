use crate::wire::DefIndex;
use crate::DefStruct;

#[derive(Debug, Clone, PartialEq, DefStruct)]
pub struct HitLocationsDef {
    #[def("HitLocations")]
    pub hit_locations: Vec<DefIndex>,
}
