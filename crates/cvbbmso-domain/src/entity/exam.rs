use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "exams")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub exam_id: String,
    pub title: String,
    pub start_at: String,
    pub end_at: String,
    pub segment_duration_secs: i64,
    pub fps: i64,
}

#[derive(Clone, Copy, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
