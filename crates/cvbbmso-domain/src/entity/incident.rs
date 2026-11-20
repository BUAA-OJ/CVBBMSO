use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "incidents")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub ordinal: i64,
    pub incident_id: String,
    pub exam_id: String,
    pub oj_user_id: i64,
    pub source: String,
    pub kind: String,
    pub occurred_at: String,
    pub detail: Option<String>,
    pub event_seq: Option<Vec<u8>>,
}

#[derive(Clone, Copy, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
