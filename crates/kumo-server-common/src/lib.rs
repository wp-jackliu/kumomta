use config::{
    any_err, decorate_callback_name, from_lua_value, get_or_create_module, load_config,
    CallbackSignature,
};
use mlua::{Function, Lua, LuaSerdeExt, Value};
use mod_redis::RedisConnKey;
use serde::Deserialize;
use tokio::task::LocalSet;

pub mod config_handle;
pub mod diagnostic_logging;
pub mod http_server;
pub mod nodeid;
pub mod panic;
pub mod start;
pub mod tls_helpers;

pub fn register(lua: &Lua) -> anyhow::Result<()> {
    for func in [
        mod_redis::register,
        data_loader::register,
        mod_digest::register,
        mod_encode::register,
        cidr_map::register,
        domain_map::register,
        mod_amqp::register,
        mod_filesystem::register,
        mod_http::register,
        mod_regex::register,
        mod_serde::register,
        mod_sqlite::register,
        mod_string::register,
        mod_dns_resolver::register,
        mod_kafka::register,
        mod_memoize::register,
        kumo_api_types::shaping::register,
        regex_set_map::register,
    ] {
        func(lua)?;
    }

    let kumo_mod = get_or_create_module(lua, "kumo")?;

    kumo_mod.set(
        "on",
        lua.create_function(move |lua, (name, func): (String, Function)| {
            let decorated_name = decorate_callback_name(&name);

            if let Ok(current_event) = lua.globals().get::<_, String>("_KUMO_CURRENT_EVENT") {
                return Err(mlua::Error::external(format!(
                    "Attempting to register an event handler via \
                    `kumo.on('{name}', ...)` from within the event handler \
                    '{current_event}'. You must move your event handler registration \
                    so that it is setup directly when the policy is loaded \
                    in order for it to consistently trigger and handle events."
                )));
            }

            if config::does_callback_allow_multiple(&name) {
                let tbl: Value = lua.named_registry_value(&decorated_name)?;
                return match tbl {
                    Value::Nil => {
                        let tbl = lua.create_table()?;
                        tbl.set(1, func)?;
                        lua.set_named_registry_value(&decorated_name, tbl)?;
                        Ok(())
                    }
                    Value::Table(tbl) => {
                        let len = tbl.raw_len();
                        tbl.set(len + 1, func)?;
                        Ok(())
                    }
                    _ => Err(mlua::Error::external(format!(
                        "registry key for {decorated_name} has invalid type",
                    ))),
                };
            }

            let existing: Value = lua.named_registry_value(&decorated_name)?;
            match existing {
                Value::Nil => {}
                Value::Function(func) => {
                    let info = func.info();
                    let src = String::from_utf8_lossy(
                        info.source.as_ref().map(|v| v.as_slice()).unwrap_or(b"?"),
                    );
                    let line = info.line_defined;
                    return Err(mlua::Error::external(format!(
                        "{name} event already has a handler defined at {src}:{line}"
                    )));
                }
                _ => {
                    return Err(mlua::Error::external(format!(
                        "{name} event already has a handler"
                    )));
                }
            }

            lua.set_named_registry_value(&decorated_name, func)?;
            Ok(())
        })?,
    )?;

    kumo_mod.set(
        "set_diagnostic_log_filter",
        lua.create_function(move |_, filter: String| {
            diagnostic_logging::set_diagnostic_log_filter(&filter).map_err(any_err)
        })?,
    )?;

    kumo_mod.set(
        "set_max_spare_lua_contexts",
        lua.create_function(move |_, limit: usize| {
            config::set_max_spare(limit);
            Ok(())
        })?,
    )?;

    kumo_mod.set(
        "set_max_lua_context_use_count",
        lua.create_function(move |_, limit: usize| {
            config::set_max_use(limit);
            Ok(())
        })?,
    )?;

    kumo_mod.set(
        "set_max_lua_context_age",
        lua.create_function(move |_, limit: usize| {
            config::set_max_age(limit);
            Ok(())
        })?,
    )?;

    kumo_mod.set(
        "available_parallelism",
        lua.create_function(move |_, _: ()| {
            Ok(std::thread::available_parallelism().map_err(any_err)?.get())
        })?,
    )?;

    kumo_mod.set(
        "configure_redis_throttles",
        lua.create_async_function(|lua, params: Value| async move {
            let key: RedisConnKey = from_lua_value(lua, params)?;
            let conn = key.open().await.map_err(any_err)?;
            throttle::use_redis(conn).map_err(any_err)
        })?,
    )?;

    kumo_mod.set(
        "sleep",
        lua.create_async_function(|_, seconds: f64| async move {
            tokio::time::sleep(tokio::time::Duration::from_secs_f64(seconds)).await;
            Ok(())
        })?,
    )?;

    // TODO: options like restarting on error, delay between
    // restarts and so on
    #[derive(Deserialize, Debug)]
    struct TaskParams {
        event_name: String,
        args: Vec<serde_json::Value>,
    }

    impl TaskParams {
        async fn run(&self) -> anyhow::Result<()> {
            let mut config = load_config().await?;

            let sig = CallbackSignature::<Value, ()>::new(self.event_name.to_string());

            config
                .convert_args_and_call_callback(&sig, &self.args)
                .await
        }
    }

    kumo_mod.set(
        "spawn_task",
        lua.create_function(|lua, params: Value| {
            let params: TaskParams = lua.from_value(params)?;

            if !config::is_validating() {
                std::thread::Builder::new()
                    .name(format!("spawned-task-{}", params.event_name))
                    .spawn(move || {
                        let runtime = tokio::runtime::Builder::new_current_thread()
                            .enable_io()
                            .enable_time()
                            .on_thread_park(|| kumo_server_memory::purge_thread_cache())
                            .build()
                            .unwrap();
                        let local_set = LocalSet::new();
                        let event_name = params.event_name.clone();

                        let result =
                            local_set.block_on(&runtime, async move { params.run().await });
                        if let Err(err) = result {
                            tracing::error!("Error while dispatching {event_name}: {err:#}");
                        }
                    })?;
            }

            Ok(())
        })?,
    )?;

    kumo_mod.set(
        "validation_failed",
        lua.create_function(|_, ()| {
            config::set_validation_failed();
            Ok(())
        })?,
    )?;

    Ok(())
}
