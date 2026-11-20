use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "recording_manifests")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub exam_id: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub oj_user_id: i64,
    pub source_seq: Vec<u8>,
    pub manifest: String,
}

#[derive(Clone, Copy, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
