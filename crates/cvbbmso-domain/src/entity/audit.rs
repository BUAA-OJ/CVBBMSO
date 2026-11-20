use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "admin_audit")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub audit_id: String,
    pub actor: String,
    pub operation: String,
    pub exam_id: String,
    pub target_users: Option<String>,
    pub change_count: i64,
    pub occurred_at: String,
}

#[derive(Clone, Copy, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
