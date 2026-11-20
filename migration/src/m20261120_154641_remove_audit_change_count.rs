use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub(super) struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let operation = Expr::col("operation");
        manager
            .execute(
                Query::update()
                    .table("admin_audit")
                    .value(
                        "operation",
                        Func::cust("json_remove")
                            .arg(operation.clone())
                            .arg("$.change_count"),
                    )
                    .and_where(
                        Expr::from(Func::cust("json_type").arg(operation).arg("$.change_count"))
                            .is_not_null(),
                    )
                    .take(),
            )
            .await
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        // The previous schema accepts audit operations without the optional count.
        Ok(())
    }
}
