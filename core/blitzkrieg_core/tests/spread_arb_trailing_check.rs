//! spread_arb 的 `trailing_exit` 声明端到端：REAL 包（manifest `trailing_exit:
//! true`）→ `LuaEngineAdapter` → `wants_trailing_exit()`；service 装配路径
//! （`declare_trailing_exit`）不在本牙，但 trait 面与装配构造器在这里钉死。
//! 簿侧的行为判定在 `position::tests::
//! a_strategy_trailing_declaration_overrides_the_kernel_value_for_its_positions`。

use blitzkrieg_core::strategies::EngineStrategy as _;
use blitzkrieg_core::strategy_engine::lua_loader::{LuaEngineAdapter, load_lua_package};
use std::path::PathBuf;

fn package_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../user_layer/strategies_lua/spread_arb")
}

#[test]
fn spread_arb_declares_trailing_exit_through_the_real_package() {
    let loaded = load_lua_package(&package_dir()).expect("spread_arb package must load");
    assert_eq!(
        loaded.trailing_exit,
        Some(true),
        "manifest trailing_exit:true must survive the loader"
    );

    let adapter = LuaEngineAdapter::new(loaded.strategy, loaded.tunables)
        .declare_trailing_exit(loaded.trailing_exit);
    assert_eq!(
        adapter.wants_trailing_exit(),
        Some(true),
        "the adapter's trait face answers the declaration"
    );
}
