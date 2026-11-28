use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "student_enrollments")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub exam_id: String,
    #[sea_orm(primary_key, auto_increment = false)]
    pub student_number: String,
    pub oj_user_id: Option<i64>,
    pub enrolled: bool,
}

#[derive(Clone, Copy, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
