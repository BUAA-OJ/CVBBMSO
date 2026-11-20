use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "incidents")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub incident_id: String,
    pub exam_id: String,
    pub oj_user_id: i64,
    pub kind: String,
    pub occurred_at: String,
    pub detail: Option<String>,
}

#[derive(Clone, Copy, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
