# 测试策略

## 分层

```
tests/
├── common/         # 集成测试共享基建（启动测试 PG、迁移、truncate、假 COS）
├── api/            # 按业务场景的集成测试（HTTP 调用 + 真实 PG）
└── unit/           # 单元测试（mock repository，零依赖）
```

## 集成测试

对应 Python 项目 `tests/conftest.py`：

1. **测试容器**：`docker compose -f docker-compose.test.yml up -d` 拉起独立 PG
   （端口 5434，bind mount `./data/postgres-test`，session 结束自动 down -v 清理）。
2. **clean_db**：每个测试函数执行 truncate 所有表 + RESTART IDENTITY + 重置流水号计数器。
3. **假 COS**：`FakeCosClient` 内存实现 + 故障注入，对应 `NoopCos` 占位。
4. **环境隔离**：强制覆盖 `DATABASE_URL` 为测试库，开发库（5433）不可被触及。

## 单元测试

service 层使用 mock repository（`mockall` 或手写 trait mock），无 Docker 依赖。

## 骨架阶段

本目录仅含占位文件 `tests/common/mod.rs`，实施阶段在此基础上补充：
- `tests/api/` 各域场景
- 各模块 `#[cfg(test)]` 单元测试块

骨架的 `cargo test` 验证不依赖数据库（无 `query!` 宏调用）。