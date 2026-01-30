use crate::DefStruct;
use crate::wire::DefIndex;
use crate::enums::HeroTrainingStatus;

#[derive(Debug, Clone, PartialEq, DefStruct)]
pub struct HeroDef {
    #[def("DefaultTitle")]
    pub default_title: DefIndex,
    #[def("DefaultHeroTrainingStatus")]
    pub default_hero_training_status: HeroTrainingStatus,
}
