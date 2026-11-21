use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "admin_audit")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub audit_id: i64,
    pub actor: String,
    pub operation: String,
    pub occurred_at: String,
}

#[derive(Clone, Copy, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
