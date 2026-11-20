use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "events")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub exam_id: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub oj_user_id: i64,
    #[sea_orm(primary_key, auto_increment = false)]
    pub seq: Vec<u8>,
    pub event: String,
    pub received_at: String,
}

#[derive(Clone, Copy, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
