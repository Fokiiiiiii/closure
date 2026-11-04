(module
  (memory (export "memory") 1)
  (data (i32.const 1024) "[{\22type\22:\22get_guild\22,\22guild_id\22:\221\22}]")
  (func (export "alloc") (param i32) (result i32) i32.const 0)
  (func (export "call") (param i32 i32) (result i64)
    i64.const 4398046511141))
