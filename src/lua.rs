//! `specialfx` for Lua scripts run by [avarice](https://github.com/lloydeverett/avarice).
//!
//! Register [`SpecialFx`] with a runtime and scripts can `require` it:
//!
//! ```lua
//! local fx = require("specialfx")
//! local overlay = fx.overlay(fx.color.rgba(1, 0.55, 0.1, 0.3))
//! overlay:set_color("#00000080")
//! overlay:close()
//! ```
//!
//! A colour is a table `{ r =, g =, b =, a = }` of numbers from 0 to 1, and
//! anywhere a colour is taken a hex string (`"#rrggbbaa"`, as
//! [`Color`]'s `FromStr`) works too. Errors, including bad arguments, are
//! raised as Lua errors.
//!
//! An overlay lasts until `close()` is called on it or it is garbage
//! collected, so a script must keep a reference to it.
//!
//! On macOS nothing shows until the main thread pumps events (see
//! [Threading](crate#threading)). That is the embedding program's job, which
//! is why the module has no `run_until`.

use avarice::mlua::{self, FromLua, IntoLua, Lua, Table, UserData, UserDataMethods, Value};
use avarice::HostModule;

use crate::{exemptions, Color, HideOthersOptions, Overlay, OverlayOptions};

/// The `specialfx` module.
pub struct SpecialFx;

impl HostModule for SpecialFx {
    fn name(&self) -> &str {
        "specialfx"
    }

    fn load(&self, lua: &Lua) -> mlua::Result<Value> {
        let module = lua.create_table()?;
        module.set("color", color_module(lua)?)?;
        module.set(
            "overlay",
            lua.create_function(|_, (color, options): (LuaColor, Option<Table>)| {
                let mut overlay_options = OverlayOptions::new(color.0);
                if let Some(options) = options {
                    expect_keys(&options, &["exclude_from_capture"])?;
                    if let Some(exclude) = optional_bool(&options, "exclude_from_capture")? {
                        overlay_options.exclude_from_capture = exclude;
                    }
                }
                Ok(LuaOverlay(Some(Overlay::new(overlay_options).map_err(lua_error)?)))
            })?,
        )?;
        module.set(
            "hide_others",
            lua.create_function(|_, options: Option<Table>| {
                let mut hide_options = HideOthersOptions::default();
                if let Some(options) = options {
                    expect_keys(&options, &["builtin_exemptions", "exempt"])?;
                    if let Some(builtin) = optional_bool(&options, "builtin_exemptions")? {
                        hide_options.builtin_exemptions = builtin;
                    }
                    match options.get::<Value>("exempt")? {
                        Value::Nil => {}
                        Value::Table(apps) => {
                            hide_options.exempt = (1..=apps.raw_len()).map(|i| expect_string(&format!("exempt[{i}]"), apps.raw_get(i)?)).collect::<mlua::Result<_>>()?;
                        }
                        other => return Err(wrong_type("exempt", "a list of strings", &other)),
                    }
                }
                crate::hide_others(&hide_options).map_err(lua_error)
            })?,
        )?;
        module.set("show_others", lua.create_function(|_, ()| crate::show_others().map_err(lua_error))?)?;
        module.set(
            "set_background_app",
            lua.create_function(|_, background: Value| crate::set_background_app(expect_bool("background", background)?).map_err(lua_error))?,
        )?;
        let exemption_lists = lua.create_table()?;
        exemption_lists.set("macos", exemptions::MACOS)?;
        exemption_lists.set("windows", exemptions::WINDOWS)?;
        module.set("exemptions", exemption_lists)?;
        Ok(Value::Table(module))
    }
}

/// `fx.color`: building colours and doing arithmetic on them, as [`Color`].
fn color_module(lua: &Lua) -> mlua::Result<Table> {
    let color = lua.create_table()?;
    color.set("transparent", LuaColor(Color::TRANSPARENT))?;
    color.set("rgba", lua.create_function(|_, (Number(r), Number(g), Number(b), Number(a))| Ok(LuaColor(Color::rgba(r, g, b, a))))?)?;
    color.set("parse", lua.create_function(|_, hex: String| hex.parse().map(LuaColor).map_err(lua_error))?)?;
    color.set("from_rgba8", lua.create_function(|_, (r, g, b, a): (u8, u8, u8, u8)| Ok(LuaColor(Color::from_rgba8(r, g, b, a))))?)?;
    color.set(
        "to_rgba8",
        lua.create_function(|_, c: LuaColor| {
            let [r, g, b, a] = c.0.to_rgba8();
            Ok((r, g, b, a))
        })?,
    )?;
    color.set("with_alpha", lua.create_function(|_, (c, Number(a)): (LuaColor, Number)| Ok(LuaColor(c.0.with_alpha(a))))?)?;
    color.set("clamped", lua.create_function(|_, c: LuaColor| Ok(LuaColor(c.0.clamped())))?)?;
    color.set("lerp", lua.create_function(|_, (from, to, Number(t)): (LuaColor, LuaColor, Number)| Ok(LuaColor(from.0.lerp(to.0, t))))?)?;
    color.set(
        "blend_over",
        lua.create_function(|lua, (c, src): (LuaColor, Table)| {
            let [r, g, b] = c.0.blend_over([number(&src, "r")?, number(&src, "g")?, number(&src, "b")?]);
            lua.create_table_from([("r", r), ("g", g), ("b", b)])
        })?,
    )?;
    color.set(
        "as_affine",
        lua.create_function(|lua, c: LuaColor| {
            let affine = lua.create_table()?;
            for (channel, (scale, offset)) in ["r", "g", "b"].into_iter().zip(c.0.as_affine()) {
                affine.set(channel, lua.create_table_from([("scale", scale), ("offset", offset)])?)?;
            }
            Ok(affine)
        })?,
    )?;
    Ok(color)
}

/// A [`Color`] as Lua sees it: a `{ r, g, b, a }` table, or a hex string on
/// the way in.
struct LuaColor(Color);

impl FromLua for LuaColor {
    fn from_lua(value: Value, _: &Lua) -> mlua::Result<Self> {
        match value {
            Value::String(hex) => hex.to_str()?.parse().map(LuaColor).map_err(lua_error),
            Value::Table(c) => Ok(LuaColor(Color::rgba(number(&c, "r")?, number(&c, "g")?, number(&c, "b")?, number(&c, "a")?))),
            other => Err(wrong_type("color", "a colour table or hex string", &other)),
        }
    }
}

impl IntoLua for LuaColor {
    fn into_lua(self, lua: &Lua) -> mlua::Result<Value> {
        let Color { r, g, b, a } = self.0;
        lua.create_table_from([("r", r), ("g", g), ("b", b), ("a", a)]).map(Value::Table)
    }
}

/// An overlay handle. `None` once closed.
struct LuaOverlay(Option<Overlay>);

impl LuaOverlay {
    /// The overlay, unless it has been closed.
    fn live(&self) -> mlua::Result<&Overlay> {
        self.0.as_ref().ok_or_else(closed)
    }

    fn live_mut(&mut self) -> mlua::Result<&mut Overlay> {
        self.0.as_mut().ok_or_else(closed)
    }
}

fn closed() -> mlua::Error {
    mlua::Error::runtime("overlay is closed")
}

impl UserData for LuaOverlay {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("color", |_, this, ()| Ok(LuaColor(this.live()?.color())));
        methods.add_method_mut("set_color", |_, this, c: LuaColor| this.live_mut()?.set_color(c.0).map_err(lua_error));
        methods.add_method_mut("close", |_, this, ()| {
            this.0 = None;
            Ok(())
        });
    }
}

/// A number argument. Unlike `f32`, it doesn't accept numeric strings, so
/// arguments are checked the same way as colour table fields.
struct Number(f32);

impl FromLua for Number {
    fn from_lua(value: Value, _: &Lua) -> mlua::Result<Self> {
        expect_number("argument", value).map(Number)
    }
}

/// The number at `key`, which must be there.
fn number(table: &Table, key: &str) -> mlua::Result<f32> {
    expect_number(key, table.get(key)?)
}

/// The boolean at `key`, if there is one. Lua's truthiness would let a typo
/// like `"false"` mean `true`, so anything else is an error.
fn optional_bool(table: &Table, key: &str) -> mlua::Result<Option<bool>> {
    match table.get::<Value>(key)? {
        Value::Nil => Ok(None),
        value => expect_bool(key, value).map(Some),
    }
}

/// Raises on keys an options table doesn't take, so a misspelt option isn't
/// silently ignored.
fn expect_keys(options: &Table, known: &[&str]) -> mlua::Result<()> {
    for pair in options.pairs::<Value, Value>() {
        let (key, _) = pair?;
        let is_known = match &key {
            Value::String(name) => known.iter().any(|known| name.as_bytes() == known.as_bytes()),
            _ => false,
        };
        if !is_known {
            return Err(mlua::Error::runtime(format!("unknown option {}, expected one of {known:?}", key.to_string()?)));
        }
    }
    Ok(())
}

fn expect_number(name: &str, value: Value) -> mlua::Result<f32> {
    match value {
        Value::Integer(n) => Ok(n as f32),
        Value::Number(n) => Ok(n as f32),
        other => Err(wrong_type(name, "a number", &other)),
    }
}

fn expect_bool(name: &str, value: Value) -> mlua::Result<bool> {
    match value {
        Value::Boolean(b) => Ok(b),
        other => Err(wrong_type(name, "a boolean", &other)),
    }
}

fn expect_string(name: &str, value: Value) -> mlua::Result<String> {
    match value {
        Value::String(s) => Ok(s.to_str()?.to_owned()),
        other => Err(wrong_type(name, "a string", &other)),
    }
}

fn wrong_type(name: &str, expected: &str, got: &Value) -> mlua::Error {
    mlua::Error::runtime(format!("{name} must be {expected}, not {}", got.type_name()))
}

/// A library error as a Lua error.
fn lua_error(e: impl std::fmt::Display) -> mlua::Error {
    mlua::Error::runtime(e.to_string())
}

#[cfg(test)]
mod tests {
    use avarice::{Profile, Runtime};

    use super::*;

    fn runtime() -> Runtime {
        Runtime::builder(Profile::Sandbox).module(SpecialFx).build().unwrap()
    }

    /// Runs `body` with `fx` bound to the module and returns its result.
    fn eval<R: mlua::FromLuaMulti>(body: &str) -> avarice::Result<R> {
        let rt = runtime();
        rt.block_on(rt.eval(format!("local fx = require('specialfx')\n{body}"), "=test"))
    }

    fn error(body: &str) -> String {
        eval::<()>(body).unwrap_err().to_string()
    }

    #[test]
    fn colors_are_tables() {
        let rgba: (f32, f32, f32, f32) = eval("local c = fx.color.rgba(0.25, 0.5, 0.75, 1) return c.r, c.g, c.b, c.a").unwrap();
        assert_eq!(rgba, (0.25, 0.5, 0.75, 1.0));
        let rgba: (f32, f32, f32, f32) = eval("local c = fx.color.transparent return c.r, c.g, c.b, c.a").unwrap();
        assert_eq!(rgba, (0.0, 0.0, 0.0, 0.0));
    }

    #[test]
    fn parse_reads_hex() {
        let rgba: (u8, u8, u8, u8) = eval("return fx.color.to_rgba8(fx.color.parse('#ff8000'))").unwrap();
        assert_eq!(rgba, (255, 128, 0, 255));
    }

    #[test]
    fn hex_strings_stand_in_for_colors() {
        let a: f32 = eval("return fx.color.with_alpha('#ff8000', 0.5).a").unwrap();
        assert_eq!(a, 0.5);
    }

    #[test]
    fn bad_colors_raise() {
        let message = error("fx.color.parse('nope')");
        assert!(message.contains("nope"), "{message}");
        assert!(error("fx.color.with_alpha('nope', 1)").contains("nope"));
        assert!(error("fx.color.with_alpha({ r = 1, g = 1, b = 1 }, 1)").contains("a must be a number, not nil"));
        assert!(error("fx.color.with_alpha(5, 1)").contains("color must be"));
    }

    #[test]
    fn color_functions_match_the_library() {
        let rgba: (u8, u8, u8, u8) = eval("return fx.color.to_rgba8(fx.color.from_rgba8(1, 2, 3, 4))").unwrap();
        assert_eq!(rgba, (1, 2, 3, 4));
        let rgba: (f32, f32, f32, f32) = eval("local c = fx.color.clamped(fx.color.rgba(2, -1, 0.5, 3)) return c.r, c.g, c.b, c.a").unwrap();
        assert_eq!(rgba, (1.0, 0.0, 0.5, 1.0));
        let r: f32 = eval("return fx.color.lerp(fx.color.rgba(0, 0, 0, 0), '#ffffffff', 0.5).r").unwrap();
        assert_eq!(r, 0.5);
    }

    #[test]
    fn blend_over_takes_and_returns_rgb() {
        let color = Color::rgba(1.0, 0.0, 0.0, 0.5);
        let [r, g, b] = color.blend_over([0.0, 1.0, 0.5]);
        let got: (f32, f32, f32) = eval("local o = fx.color.blend_over(fx.color.rgba(1, 0, 0, 0.5), { r = 0, g = 1, b = 0.5 }) return o.r, o.g, o.b").unwrap();
        assert_eq!(got, (r, g, b));
    }

    #[test]
    fn as_affine_gives_scale_and_offset_per_channel() {
        let [(scale, offset), ..] = Color::rgba(1.0, 0.0, 0.0, 0.25).as_affine();
        let got: (f32, f32) = eval("local m = fx.color.as_affine(fx.color.rgba(1, 0, 0, 0.25)) return m.r.scale, m.r.offset").unwrap();
        assert_eq!(got, (scale, offset));
    }

    #[test]
    fn exemptions_list_the_builtin_apps() {
        let first: String = eval("return fx.exemptions.macos[1]").unwrap();
        assert_eq!(first, crate::exemptions::MACOS[0]);
        let n: usize = eval("return #fx.exemptions.windows").unwrap();
        assert_eq!(n, crate::exemptions::WINDOWS.len());
    }

    #[test]
    fn bad_options_raise() {
        assert!(error("fx.hide_others({ exempt = 5 })").contains("exempt must be"));
        assert!(error("fx.hide_others({ builtin_exemptions = 'yes' })").contains("builtin_exemptions must be"));
        assert!(error("fx.overlay('#000', { exclude_from_capture = 'no' })").contains("exclude_from_capture must be"));
        assert!(error("fx.hide_others({ exempt = { 'a', 5 } })").contains("exempt[2] must be a string"));
    }

    #[test]
    fn misspelt_options_raise() {
        assert!(error("fx.overlay('#000', { exclude_from_capure = true })").contains("unknown option exclude_from_capure"));
        assert!(error("fx.hide_others({ exmept = {} })").contains("unknown option exmept"));
    }

    #[test]
    fn numeric_strings_are_not_numbers() {
        assert!(error("fx.color.rgba('0.5', 0, 0, 1)").contains("must be a number, not string"));
        assert!(error("fx.color.with_alpha('#000', '1')").contains("must be a number, not string"));
    }

    #[test]
    fn overlay_needs_a_color() {
        assert!(error("fx.overlay()").contains("color must be"));
    }

    #[test]
    fn show_others_without_hide_others_does_nothing() {
        eval::<()>("fx.show_others()").unwrap();
    }

    // As in the library's own tests: off the main thread, macOS queues the
    // windows rather than failing.
    #[cfg(target_os = "macos")]
    #[test]
    fn overlays_can_be_recoloured_and_closed() {
        let a: f32 = eval("local o = fx.overlay('#ff800080') o:set_color(fx.color.rgba(0, 0, 0, 2)) local a = o:color().a o:close() return a").unwrap();
        assert_eq!(a, 1.0);
        assert!(error("local o = fx.overlay('#000', { exclude_from_capture = false }) o:close() o:color()").contains("closed"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn set_background_app_takes_a_bool() {
        eval::<()>("fx.set_background_app(true)").unwrap();
        assert!(error("fx.set_background_app('yes')").contains("must be a boolean"));
    }
}
