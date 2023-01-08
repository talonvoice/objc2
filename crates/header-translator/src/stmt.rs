use std::borrow::Cow;
use std::collections::HashSet;
use std::fmt;
use std::iter;
use std::mem;

use clang::{Entity, EntityKind, EntityVisitResult};
use tracing::{debug, debug_span, error, warn};

use crate::availability::Availability;
use crate::config::ClassData;
use crate::context::Context;
use crate::expr::Expr;
use crate::immediate_children;
use crate::method::{handle_reserved, Method};
use crate::rust_type::{Ownership, Ty};
use crate::unexposed_macro::UnexposedMacro;

#[derive(serde::Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Derives(Cow<'static, str>);

impl Default for Derives {
    fn default() -> Self {
        Derives("Debug, PartialEq, Eq, Hash".into())
    }
}

impl fmt::Display for Derives {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.0.is_empty() {
            write!(f, "#[derive({})]", self.0)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ClassDefReference {
    pub name: String,
    pub generics: Vec<String>,
}

impl ClassDefReference {
    pub fn new(name: String, generics: Vec<String>) -> Self {
        Self { name, generics }
    }
}

fn parse_superclass<'ty>(entity: &Entity<'ty>) -> Option<(Entity<'ty>, ClassDefReference)> {
    let mut superclass = None;
    let mut generics = Vec::new();

    immediate_children(entity, |entity, _span| match entity.get_kind() {
        EntityKind::ObjCSuperClassRef => {
            superclass = Some(entity);
        }
        EntityKind::TypeRef => {
            let name = entity.get_name().expect("typeref name");
            generics.push(name);
        }
        _ => {}
    });

    superclass.map(|entity| {
        (
            entity
                .get_reference()
                .expect("ObjCSuperClassRef to reference entity"),
            ClassDefReference::new(entity.get_name().expect("superclass name"), generics),
        )
    })
}

/// Takes one of:
/// - `EntityKind::ObjCInterfaceDecl`
/// - `EntityKind::ObjCProtocolDecl`
/// - `EntityKind::ObjCCategoryDecl`
fn parse_objc_decl(
    entity: &Entity<'_>,
    superclass: bool,
    mut generics: Option<&mut Vec<String>>,
    data: Option<&ClassData>,
    context: &Context<'_>,
) -> (Vec<String>, Vec<Method>, Vec<String>) {
    let mut protocols = Vec::new();
    let mut methods = Vec::new();
    let mut designated_initializers = Vec::new();

    // Track seen properties, so that when methods are autogenerated by the
    // compiler from them, we can skip them
    let mut properties = HashSet::new();

    immediate_children(entity, |entity, span| match entity.get_kind() {
        EntityKind::ObjCExplicitProtocolImpl if generics.is_none() && !superclass => {
            // TODO NS_PROTOCOL_REQUIRES_EXPLICIT_IMPLEMENTATION
        }
        EntityKind::ObjCIvarDecl if superclass => {
            // Explicitly ignored
        }
        EntityKind::ObjCSuperClassRef | EntityKind::TypeRef if superclass => {
            // Parsed in parse_superclass
        }
        EntityKind::ObjCRootClass => {
            debug!("parsing root class");
        }
        EntityKind::ObjCClassRef if generics.is_some() => {
            // debug!("ObjCClassRef: {:?}", entity.get_display_name());
        }
        EntityKind::TemplateTypeParameter => {
            if let Some(generics) = &mut generics {
                // TODO: Generics with bounds (like NSMeasurement<UnitType: NSUnit *>)
                // let ty = entity.get_type().expect("template type");
                let name = entity.get_display_name().expect("template name");
                generics.push(name);
            } else {
                error!("unsupported generics");
            }
        }
        EntityKind::ObjCProtocolRef => {
            protocols.push(entity.get_name().expect("protocolref to have name"));
        }
        EntityKind::ObjCInstanceMethodDecl | EntityKind::ObjCClassMethodDecl => {
            drop(span);
            let partial = Method::partial(entity);

            if !properties.remove(&(partial.is_class, partial.fn_name.clone())) {
                let data = ClassData::get_method_data(data, &partial.fn_name);
                if let Some((designated_initializer, method)) = partial.parse(data, context) {
                    if designated_initializer {
                        designated_initializers.push(method.fn_name.clone());
                    }
                    methods.push(method);
                }
            }
        }
        EntityKind::ObjCPropertyDecl => {
            drop(span);
            let partial = Method::partial_property(entity);

            assert!(
                properties.insert((partial.is_class, partial.getter_name.clone())),
                "already exisiting property"
            );
            if let Some(setter_name) = partial.setter_name.clone() {
                assert!(
                    properties.insert((partial.is_class, setter_name)),
                    "already exisiting property"
                );
            }

            let getter_data = ClassData::get_method_data(data, &partial.getter_name);
            let setter_data = partial
                .setter_name
                .as_ref()
                .map(|setter_name| ClassData::get_method_data(data, setter_name));

            let (getter, setter) = partial.parse(getter_data, setter_data, context);
            if let Some(getter) = getter {
                methods.push(getter);
            }
            if let Some(setter) = setter {
                methods.push(setter);
            }
        }
        EntityKind::VisibilityAttr => {
            // Already exposed as entity.get_visibility()
        }
        EntityKind::ObjCException if superclass => {
            // Maybe useful for knowing when to implement `Error` for the type
        }
        EntityKind::UnexposedAttr => {
            if let Some(macro_) = UnexposedMacro::parse(&entity) {
                warn!(?macro_, "unknown macro");
            }
        }
        _ => warn!("unknown"),
    });

    if !properties.is_empty() {
        if properties == HashSet::from([(false, "setDisplayName".to_owned())]) {
            // TODO
        } else {
            error!(
                ?methods,
                ?properties,
                "did not properly add methods to properties"
            );
        }
    }

    (protocols, methods, designated_initializers)
}

#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    /// @interface name: superclass <protocols*>
    /// ->
    /// extern_class!
    ClassDecl {
        ty: ClassDefReference,
        availability: Availability,
        superclasses: Vec<ClassDefReference>,
        designated_initializers: Vec<String>,
        derives: Derives,
        ownership: Ownership,
    },
    /// @interface class_name (name) <protocols*>
    /// ->
    /// extern_methods!
    Methods {
        ty: ClassDefReference,
        availability: Availability,
        methods: Vec<Method>,
        /// For the categories that have a name (though some don't, see NSClipView)
        category_name: Option<String>,
        description: Option<String>,
    },
    /// @protocol name <protocols*>
    /// ->
    /// extern_protocol!
    ProtocolDecl {
        name: String,
        availability: Availability,
        protocols: Vec<String>,
        methods: Vec<Method>,
    },
    /// @interface ty: _ <protocols*>
    /// @interface ty (_) <protocols*>
    ProtocolImpl {
        ty: ClassDefReference,
        availability: Availability,
        protocol: String,
    },
    /// struct name {
    ///     fields*
    /// };
    ///
    /// typedef struct {
    ///     fields*
    /// } name;
    ///
    /// typedef struct _name {
    ///     fields*
    /// } name;
    StructDecl {
        name: String,
        boxable: bool,
        fields: Vec<(String, Ty)>,
    },
    /// typedef NS_OPTIONS(type, name) {
    ///     variants*
    /// };
    ///
    /// typedef NS_ENUM(type, name) {
    ///     variants*
    /// };
    ///
    /// enum name {
    ///     variants*
    /// };
    ///
    /// enum {
    ///     variants*
    /// };
    EnumDecl {
        name: Option<String>,
        ty: Ty,
        kind: Option<UnexposedMacro>,
        variants: Vec<(String, Expr)>,
    },
    /// static const ty name = expr;
    /// extern const ty name;
    VarDecl {
        name: String,
        ty: Ty,
        value: Option<Expr>,
    },
    /// extern ret name(args*);
    ///
    /// static inline ret name(args*) {
    ///     body
    /// }
    FnDecl {
        name: String,
        arguments: Vec<(String, Ty)>,
        result_type: Ty,
        // Some -> inline function.
        body: Option<()>,
    },
    /// typedef Type TypedefName;
    AliasDecl {
        name: String,
        ty: Ty,
        kind: Option<UnexposedMacro>,
    },
}

fn parse_struct(entity: &Entity<'_>, context: &Context<'_>) -> (bool, Vec<(String, Ty)>) {
    let mut boxable = false;
    let mut fields = Vec::new();

    immediate_children(entity, |entity, span| match entity.get_kind() {
        EntityKind::UnexposedAttr => {
            if let Some(macro_) = UnexposedMacro::parse(&entity) {
                warn!(?macro_, "unknown macro");
            }
        }
        EntityKind::FieldDecl => {
            drop(span);
            let name = entity.get_name().expect("struct field name");
            let _span = debug_span!("field", name).entered();

            let ty = entity.get_type().expect("struct field type");
            let ty = Ty::parse_struct_field(ty, context);

            if entity.is_bit_field() {
                error!("unsound struct bitfield");
            }

            fields.push((name, ty))
        }
        EntityKind::ObjCBoxable => {
            boxable = true;
        }
        _ => warn!("unknown"),
    });

    (boxable, fields)
}

impl Stmt {
    pub fn parse(entity: &Entity<'_>, context: &Context<'_>) -> Vec<Self> {
        let _span = debug_span!(
            "stmt",
            kind = ?entity.get_kind(),
            dbg = entity.get_name(),
        )
        .entered();

        match entity.get_kind() {
            // These are inconsequential for us, since we resolve imports differently
            EntityKind::ObjCClassRef | EntityKind::ObjCProtocolRef => vec![],
            EntityKind::ObjCInterfaceDecl => {
                // entity.get_mangled_objc_names()
                let name = entity.get_name().expect("class name");
                let class_data = context.class_data.get(&name);

                if class_data.map(|data| data.skipped).unwrap_or_default() {
                    return vec![];
                }

                let availability = Availability::parse(
                    entity
                        .get_platform_availability()
                        .expect("class availability"),
                );
                let mut generics = Vec::new();

                let (protocols, methods, designated_initializers) =
                    parse_objc_decl(entity, true, Some(&mut generics), class_data, context);

                let ty = ClassDefReference::new(name, generics);

                let mut superclass_entity = *entity;
                let mut superclasses = vec![];

                while let Some((next_entity, superclass)) = parse_superclass(&superclass_entity) {
                    superclass_entity = next_entity;
                    superclasses.push(superclass);
                }

                let methods = Self::Methods {
                    ty: ty.clone(),
                    availability: availability.clone(),
                    methods,
                    category_name: None,
                    description: None,
                };

                if !class_data
                    .map(|data| data.definition_skipped)
                    .unwrap_or_default()
                {
                    iter::once(Self::ClassDecl {
                        ty: ty.clone(),
                        availability: availability.clone(),
                        superclasses,
                        designated_initializers,
                        derives: class_data
                            .map(|data| data.derives.clone())
                            .unwrap_or_default(),
                        ownership: class_data
                            .map(|data| data.ownership.clone())
                            .unwrap_or_default(),
                    })
                    .chain(protocols.into_iter().map(|protocol| Self::ProtocolImpl {
                        ty: ty.clone(),
                        availability: availability.clone(),
                        protocol,
                    }))
                    .chain(iter::once(methods))
                    .collect()
                } else {
                    vec![methods]
                }
            }
            EntityKind::ObjCCategoryDecl => {
                let name = entity.get_name();
                let availability = Availability::parse(
                    entity
                        .get_platform_availability()
                        .expect("category availability"),
                );

                let mut class_name = None;
                entity.visit_children(|entity, _parent| {
                    if entity.get_kind() == EntityKind::ObjCClassRef {
                        if class_name.is_some() {
                            panic!("could not find unique category class")
                        }
                        class_name = Some(entity.get_name().expect("class name"));
                        EntityVisitResult::Break
                    } else {
                        EntityVisitResult::Continue
                    }
                });
                let class_name = class_name.expect("could not find category class");
                let class_data = context.class_data.get(&class_name);

                if class_data.map(|data| data.skipped).unwrap_or_default() {
                    return vec![];
                }

                let mut class_generics = Vec::new();

                let (protocols, methods, designated_initializers) = parse_objc_decl(
                    entity,
                    false,
                    Some(&mut class_generics),
                    class_data,
                    context,
                );

                if !designated_initializers.is_empty() {
                    warn!(
                        ?designated_initializers,
                        "designated initializer in category"
                    )
                }

                let ty = ClassDefReference::new(class_name, class_generics);

                iter::once(Self::Methods {
                    ty: ty.clone(),
                    availability: availability.clone(),
                    methods,
                    category_name: name,
                    description: None,
                })
                .chain(protocols.into_iter().map(|protocol| Self::ProtocolImpl {
                    ty: ty.clone(),
                    availability: availability.clone(),
                    protocol,
                }))
                .collect()
            }
            EntityKind::ObjCProtocolDecl => {
                let name = entity.get_name().expect("protocol name");
                let protocol_data = context.protocol_data.get(&name);

                if protocol_data.map(|data| data.skipped).unwrap_or_default() {
                    return vec![];
                }

                let availability = Availability::parse(
                    entity
                        .get_platform_availability()
                        .expect("protocol availability"),
                );

                let (protocols, methods, designated_initializers) =
                    parse_objc_decl(entity, false, None, protocol_data, context);

                if !designated_initializers.is_empty() {
                    warn!(
                        ?designated_initializers,
                        "designated initializer in protocol"
                    )
                }

                vec![Self::ProtocolDecl {
                    name,
                    availability,
                    protocols,
                    methods,
                }]
            }
            EntityKind::TypedefDecl => {
                let name = entity.get_name().expect("typedef name");
                let mut struct_ = None;
                let mut skip_struct = false;
                let mut kind = None;

                immediate_children(entity, |entity, _span| match entity.get_kind() {
                    EntityKind::UnexposedAttr => {
                        if let Some(macro_) = UnexposedMacro::parse_plus_macros(&entity, context) {
                            if kind.is_some() {
                                panic!("got multiple unexposed macros {kind:?}, {macro_:?}");
                            }
                            kind = Some(macro_);
                        }
                    }
                    EntityKind::StructDecl => {
                        if context
                            .struct_data
                            .get(&name)
                            .map(|data| data.skipped)
                            .unwrap_or_default()
                        {
                            skip_struct = true;
                            return;
                        }

                        let struct_name = entity.get_name();
                        if struct_name
                            .map(|name| name.starts_with('_'))
                            .unwrap_or(true)
                        {
                            // If this struct doesn't have a name, or the
                            // name is private, let's parse it with the
                            // typedef name.
                            struct_ = Some(parse_struct(&entity, context))
                        } else {
                            skip_struct = true;
                        }
                    }
                    EntityKind::ObjCClassRef
                    | EntityKind::ObjCProtocolRef
                    | EntityKind::TypeRef
                    | EntityKind::ParmDecl => {}
                    _ => warn!("unknown"),
                });

                if let Some((boxable, fields)) = struct_ {
                    assert_eq!(kind, None, "should not have parsed a kind");
                    return vec![Stmt::StructDecl {
                        name,
                        boxable,
                        fields,
                    }];
                }

                if skip_struct {
                    return vec![];
                }

                if context
                    .typedef_data
                    .get(&name)
                    .map(|data| data.skipped)
                    .unwrap_or_default()
                {
                    return vec![];
                }

                let ty = entity
                    .get_typedef_underlying_type()
                    .expect("typedef underlying type");
                if let Some(ty) = Ty::parse_typedef(ty, &name, context) {
                    vec![Self::AliasDecl { name, ty, kind }]
                } else {
                    vec![]
                }
            }
            EntityKind::StructDecl => {
                if let Some(name) = entity.get_name() {
                    if context
                        .struct_data
                        .get(&name)
                        .map(|data| data.skipped)
                        .unwrap_or_default()
                    {
                        return vec![];
                    }

                    // See https://github.com/rust-lang/rust-bindgen/blob/95fd17b874910184cc0fcd33b287fa4e205d9d7a/bindgen/ir/comp.rs#L1392-L1408
                    if !entity.is_definition() {
                        return vec![];
                    }

                    if !name.starts_with('_') {
                        let (boxable, fields) = parse_struct(entity, context);
                        return vec![Stmt::StructDecl {
                            name,
                            boxable,
                            fields,
                        }];
                    }
                }
                vec![]
            }
            EntityKind::EnumDecl => {
                // Enum declarations show up twice for some reason, but
                // luckily this flag is set on the least descriptive entity.
                if !entity.is_definition() {
                    return vec![];
                }

                let name = entity.get_name();

                let data = context
                    .enum_data
                    .get(name.as_deref().unwrap_or("anonymous"))
                    .cloned()
                    .unwrap_or_default();
                if data.skipped {
                    return vec![];
                }

                let ty = entity.get_enum_underlying_type().expect("enum type");
                let is_signed = ty.is_signed_integer();
                let ty = Ty::parse_enum(ty, context);
                let mut kind = None;
                let mut variants = Vec::new();

                immediate_children(entity, |entity, _span| match entity.get_kind() {
                    EntityKind::EnumConstantDecl => {
                        let name = entity.get_name().expect("enum constant name");

                        if data
                            .constants
                            .get(&name)
                            .map(|data| data.skipped)
                            .unwrap_or_default()
                        {
                            return;
                        }

                        let pointer_width =
                            entity.get_translation_unit().get_target().pointer_width;

                        let val = Expr::from_val(
                            entity
                                .get_enum_constant_value()
                                .expect("enum constant value"),
                            is_signed,
                            pointer_width,
                        );
                        let expr = if data.use_value {
                            val
                        } else {
                            Expr::parse_enum_constant(&entity).unwrap_or(val)
                        };
                        variants.push((name, expr));
                    }
                    EntityKind::UnexposedAttr => {
                        if let Some(macro_) = UnexposedMacro::parse(&entity) {
                            if let Some(kind) = &kind {
                                assert_eq!(kind, &macro_, "got differing enum kinds in {name:?}");
                            } else {
                                kind = Some(macro_);
                            }
                        }
                    }
                    EntityKind::FlagEnum => {
                        let macro_ = UnexposedMacro::Options;
                        if let Some(kind) = &kind {
                            assert_eq!(kind, &macro_, "got differing enum kinds in {name:?}");
                        } else {
                            kind = Some(macro_);
                        }
                    }
                    _ => {
                        panic!("unknown enum child {entity:?} in {name:?}");
                    }
                });

                if name.is_none() && variants.is_empty() {
                    return vec![];
                }

                vec![Self::EnumDecl {
                    name,
                    ty,
                    kind,
                    variants,
                }]
            }
            EntityKind::VarDecl => {
                let name = entity.get_name().expect("var decl name");

                if context
                    .statics
                    .get(&name)
                    .map(|data| data.skipped)
                    .unwrap_or_default()
                {
                    return vec![];
                }

                let ty = entity.get_type().expect("var type");
                let ty = Ty::parse_static(ty, context);
                let mut value = None;

                immediate_children(entity, |entity, _span| match entity.get_kind() {
                    EntityKind::UnexposedAttr => {
                        if let Some(macro_) = UnexposedMacro::parse(&entity) {
                            panic!("unexpected attribute: {macro_:?}");
                        }
                    }
                    EntityKind::VisibilityAttr => {}
                    EntityKind::ObjCClassRef => {}
                    EntityKind::TypeRef => {}
                    _ if entity.is_expression() => {
                        if value.is_none() {
                            value = Some(Expr::parse_var(&entity));
                        } else {
                            panic!("got variable value twice")
                        }
                    }
                    _ => panic!("unknown vardecl child in {name}: {entity:?}"),
                });

                let value = match value {
                    Some(Some(expr)) => Some(expr),
                    Some(None) => {
                        warn!("skipped static");
                        return vec![];
                    }
                    None => None,
                };

                vec![Self::VarDecl { name, ty, value }]
            }
            EntityKind::FunctionDecl => {
                let name = entity.get_name().expect("function name");

                if context
                    .fns
                    .get(&name)
                    .map(|data| data.skipped)
                    .unwrap_or_default()
                {
                    return vec![];
                }

                if entity.is_variadic() {
                    warn!("can't handle variadic function");
                    return vec![];
                }

                let result_type = entity.get_result_type().expect("function result type");
                let result_type = Ty::parse_function_return(result_type, context);
                let mut arguments = Vec::new();

                if entity.is_static_method() {
                    warn!("unexpected static method");
                }

                immediate_children(entity, |entity, _span| match entity.get_kind() {
                    EntityKind::UnexposedAttr => {
                        if let Some(macro_) = UnexposedMacro::parse(&entity) {
                            warn!(?macro_, "unknown macro");
                        }
                    }
                    EntityKind::ObjCClassRef | EntityKind::TypeRef => {}
                    EntityKind::ParmDecl => {
                        // Could also be retrieved via. `get_arguments`
                        let name = entity.get_name().unwrap_or_else(|| "_".into());
                        let ty = entity.get_type().expect("function argument type");
                        let ty = Ty::parse_function_argument(ty, context);
                        arguments.push((name, ty))
                    }
                    EntityKind::VisibilityAttr => {
                        // CG_EXTERN or UIKIT_EXTERN
                    }
                    _ => warn!("unknown"),
                });

                let body = if entity.is_inline_function() {
                    Some(())
                } else {
                    None
                };

                vec![Self::FnDecl {
                    name,
                    arguments,
                    result_type,
                    body,
                }]
            }
            EntityKind::UnionDecl => {
                // debug!(
                //     "union: {:?}, {:?}, {:#?}, {:#?}",
                //     entity.get_display_name(),
                //     entity.get_name(),
                //     entity.has_attributes(),
                //     entity.get_children(),
                // );
                vec![]
            }
            _ => {
                panic!("Unknown: {entity:?}")
            }
        }
    }

    pub fn compare(&self, other: &Self) {
        if self != other {
            if let (
                Self::Methods {
                    methods: self_methods,
                    ..
                },
                Self::Methods {
                    methods: other_methods,
                    ..
                },
            ) = (&self, &other)
            {
                super::compare_slice(
                    self_methods,
                    other_methods,
                    |i, self_method, other_method| {
                        let _span = debug_span!("method", i).entered();
                        assert_eq!(self_method, other_method, "methods were not equal");
                    },
                );
            }

            panic!("statements were not equal:\n{self:#?}\n{other:#?}");
        }
    }
}

impl fmt::Display for Stmt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let _span = debug_span!("stmt", discriminant = ?mem::discriminant(self)).entered();

        struct GenericTyHelper<'a>(&'a ClassDefReference);

        impl fmt::Display for GenericTyHelper<'_> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0.name)?;
                if !self.0.generics.is_empty() {
                    write!(f, "<")?;
                    for generic in &self.0.generics {
                        write!(f, "{generic}, ")?;
                    }
                    for generic in &self.0.generics {
                        write!(f, "{generic}Ownership, ")?;
                    }
                    write!(f, ">")?;
                }
                Ok(())
            }
        }

        struct GenericParamsHelper<'a>(&'a [String]);

        impl fmt::Display for GenericParamsHelper<'_> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                if !self.0.is_empty() {
                    write!(f, "<")?;
                    for generic in self.0 {
                        write!(f, "{generic}: Message, ")?;
                    }
                    for generic in self.0 {
                        write!(f, "{generic}Ownership: Ownership, ")?;
                    }
                    write!(f, ">")?;
                }
                Ok(())
            }
        }

        match self {
            Self::ClassDecl {
                ty,
                availability: _,
                superclasses,
                designated_initializers: _,
                derives,
                ownership: _,
            } => {
                // TODO: Use ty.get_objc_protocol_declarations()

                let macro_name = if ty.generics.is_empty() {
                    "extern_class"
                } else {
                    "__inner_extern_class"
                };

                writeln!(f, "{macro_name}!(")?;
                writeln!(f, "    {derives}")?;
                write!(f, "    pub struct ")?;
                if ty.generics.is_empty() {
                    write!(f, "{}", ty.name)?;
                } else {
                    write!(f, "{}<", ty.name)?;
                    for generic in &ty.generics {
                        write!(f, "{generic}: Message = Object, ")?;
                    }
                    for generic in &ty.generics {
                        write!(f, "{generic}Ownership: Ownership = Shared, ")?;
                    }
                    write!(f, ">")?;
                };
                if ty.generics.is_empty() {
                    writeln!(f, ";")?;
                } else {
                    writeln!(f, " {{")?;
                    for (i, generic) in ty.generics.iter().enumerate() {
                        // Invariant over the generic (for now)
                        writeln!(
                            f,
                            "_inner{i}: PhantomData<*mut ({generic}, {generic}Ownership)>,"
                        )?;
                    }
                    writeln!(f, "notunwindsafe: PhantomData<&'static mut ()>,")?;
                    writeln!(f, "}}")?;
                }
                writeln!(f)?;
                writeln!(
                    f,
                    "    unsafe impl{} ClassType for {} {{",
                    GenericParamsHelper(&ty.generics),
                    GenericTyHelper(ty)
                )?;
                let (superclass, rest) = superclasses.split_at(1);
                let superclass = superclass.get(0).expect("must have a least one superclass");
                if !rest.is_empty() {
                    write!(f, "    #[inherits(")?;
                    let mut iter = rest.iter();
                    // Using generics in here is not technically correct, but
                    // should work for our use-cases.
                    if let Some(superclass) = iter.next() {
                        write!(f, "{}", GenericTyHelper(superclass))?;
                    }
                    for superclass in iter {
                        write!(f, ", {}", GenericTyHelper(superclass))?;
                    }
                    writeln!(f, ")]")?;
                }
                writeln!(f, "        type Super = {};", GenericTyHelper(superclass))?;
                writeln!(f, "    }}")?;
                writeln!(f, ");")?;
            }
            Self::Methods {
                ty,
                availability: _,
                methods,
                category_name,
                description,
            } => {
                writeln!(f, "extern_methods!(")?;
                if let Some(description) = description {
                    writeln!(f, "    /// {description}")?;
                    if category_name.is_some() {
                        writeln!(f, "    ///")?;
                    }
                }
                if let Some(category_name) = category_name {
                    writeln!(f, "    /// {category_name}")?;
                }
                writeln!(
                    f,
                    "    unsafe impl{} {} {{",
                    GenericParamsHelper(&ty.generics),
                    GenericTyHelper(ty)
                )?;
                for method in methods {
                    writeln!(f, "{method}")?;
                }
                writeln!(f, "    }}")?;
                writeln!(f, ");")?;
            }
            Self::ProtocolImpl {
                ty: _,
                availability: _,
                protocol: _,
            } => {
                // TODO
            }
            Self::ProtocolDecl {
                name,
                availability: _,
                protocols: _,
                methods,
            } => {
                writeln!(f, "extern_protocol!(")?;
                writeln!(f, "    pub struct {name};")?;
                writeln!(f)?;
                writeln!(f, "    unsafe impl ProtocolType for {name} {{")?;
                for method in methods {
                    writeln!(f, "{method}")?;
                }
                writeln!(f, "    }}")?;
                writeln!(f, ");")?;
            }
            Self::StructDecl {
                name,
                boxable: _,
                fields,
            } => {
                writeln!(f, "extern_struct!(")?;
                writeln!(f, "    pub struct {name} {{")?;
                for (name, ty) in fields {
                    write!(f, "        ")?;
                    if !name.starts_with('_') {
                        write!(f, "pub ")?;
                    }
                    writeln!(f, "{name}: {ty},")?;
                }
                writeln!(f, "    }}")?;
                writeln!(f, ");")?;
            }
            Self::EnumDecl {
                name,
                ty,
                kind,
                variants,
            } => {
                let macro_name = match kind {
                    None => "extern_enum",
                    Some(UnexposedMacro::Enum) => "ns_enum",
                    Some(UnexposedMacro::Options) => "ns_options",
                    Some(UnexposedMacro::ClosedEnum) => "ns_closed_enum",
                    Some(UnexposedMacro::ErrorEnum) => "ns_error_enum",
                    _ => panic!("invalid enum kind"),
                };
                writeln!(f, "{macro_name}!(")?;
                writeln!(f, "    #[underlying({ty})]")?;
                write!(f, "    pub enum ",)?;
                if let Some(name) = name {
                    write!(f, "{name} ")?;
                }
                writeln!(f, "{{")?;
                for (name, expr) in variants {
                    writeln!(f, "        {name} = {expr},")?;
                }
                writeln!(f, "    }}")?;
                writeln!(f, ");")?;
            }
            Self::VarDecl {
                name,
                ty,
                value: None,
            } => {
                writeln!(f, "extern_static!({name}: {ty});")?;
            }
            Self::VarDecl {
                name,
                ty,
                value: Some(expr),
            } => {
                writeln!(f, "extern_static!({name}: {ty} = {expr});")?;
            }
            Self::FnDecl {
                name,
                arguments,
                result_type,
                body: None,
            } => {
                writeln!(f, "extern_fn!(")?;
                write!(f, "    pub unsafe fn {name}(")?;
                for (param, arg_ty) in arguments {
                    write!(f, "{}: {arg_ty},", handle_reserved(param))?;
                }
                writeln!(f, "){result_type};")?;
                writeln!(f, ");")?;
            }
            Self::FnDecl {
                name,
                arguments,
                result_type,
                body: Some(_body),
            } => {
                writeln!(f, "inline_fn!(")?;
                write!(f, "    pub unsafe fn {name}(")?;
                for (param, arg_ty) in arguments {
                    write!(f, "{}: {arg_ty},", handle_reserved(param))?;
                }
                writeln!(f, "){result_type} {{")?;
                writeln!(f, "        todo!()")?;
                writeln!(f, "    }}")?;
                writeln!(f, ");")?;
            }
            Self::AliasDecl { name, ty, kind } => {
                match kind {
                    Some(UnexposedMacro::TypedEnum) => {
                        writeln!(f, "typed_enum!(pub type {name} = {ty};);")?;
                    }
                    Some(UnexposedMacro::TypedExtensibleEnum) => {
                        writeln!(f, "typed_extensible_enum!(pub type {name} = {ty};);")?;
                    }
                    None | Some(UnexposedMacro::BridgedTypedef) => {
                        // "bridged" typedefs should just use a normal type
                        // alias.
                        writeln!(f, "pub type {name} = {ty};")?;
                    }
                    kind => panic!("invalid alias kind {kind:?} for {ty:?}"),
                }
            }
        };
        Ok(())
    }
}
