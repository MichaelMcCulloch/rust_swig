use std::{cell::RefCell, collections::HashMap, rc::Rc, str::FromStr};

use log::{debug, trace};
use proc_macro2::{Ident, Span, TokenStream};
use quote::{quote, quote_spanned, ToTokens};
use syn::{parse_quote, punctuated::Punctuated, spanned::Spanned, Item, ItemMod, Token, Type};

use crate::{
    ast::{normalize_ty_lifetimes, GenericTypeConv, RustType},
    error::{DiagnosticError, Result},
    typemap::{
        make_unique_rust_typename_if_need, validate_code_template, TypeConvEdge, TypeMap,
        TypesConvGraph,
    },
};

static MOD_NAME_WITH_FOREIGN_TYPES: &str = "swig_foreign_types_map";
static SWIG_FOREIGNER_TYPE: &str = "swig_foreigner_type";
static SWIG_RUST_TYPE: &str = "swig_rust_type";

static SWIG_TO_FOREIGNER_HINT: &str = "swig_to_foreigner_hint";
static SWIG_FROM_FOREIGNER_HINT: &str = "swig_from_foreigner_hint";
static SWIG_CODE: &str = "swig_code";
static SWIG_GENERIC_ARG: &str = "swig_generic_arg";
static SWIG_FROM_ATTR_NAME: &str = "swig_from";
static SWIG_TO_ATTR_NAME: &str = "swig_to";

static SWIG_INTO_TRAIT: &str = "SwigInto";
static SWIG_FROM_TRAIT: &str = "SwigFrom";
static SWIG_DEREF_TRAIT: &str = "SwigDeref";
static SWIG_DEREF_MUT_TRAIT: &str = "SwigDerefMut";
static TARGET_ASSOC_TYPE: &str = "Target";

type MyAttrs = HashMap<String, Vec<(String, Span)>>;

/// # Panics
///
/// Panics if parse failed
pub(in crate::typemap) fn parse(
    name: &str,
    code: &str,
    target_pointer_width: usize,
    traits_usage_code: HashMap<Ident, String>,
) -> TypeMap {
    do_parse(name, code, target_pointer_width, traits_usage_code).unwrap_or_else(|err| {
        report_parse_error(name, code, &err);
    })
}

#[cfg(procmacro2_semver_exempt)]
fn report_parse_error(name: &str, code: &str, err: &DiagnosticError) -> ! {
    let span = err.span();
    let start = span.start();
    let end = span.end();

    let mut code_problem = String::new();
    for (i, line) in code.lines().enumerate() {
        if i == start.line {
            code_problem.push_str(if i == end.line {
                &line[start.column..end.column]
            } else {
                &line[start.column..]
            });
        } else if i > start.line && i < end.line {
            code_problem.push_str(line);
        } else if i == end.line {
            code_problem.push_str(&line[..end.column]);
            break;
        }
    }

    panic!(
        "parsing of types map '{}' failed\nerror: {}\n{}",
        name, err, code_problem
    );
}

#[cfg(not(procmacro2_semver_exempt))]
fn report_parse_error(name: &str, _code: &str, err: &DiagnosticError) -> ! {
    panic!("parsing of types map '{}' failed\nerror: '{}'", name, err);
}

fn do_parse(
    name: &str,
    code: &str,
    target_pointer_width: usize,
    traits_usage_code: HashMap<Ident, String>,
) -> Result<TypeMap> {
    let file = syn::parse_str::<syn::File>(code)?;

    let sym_foreign_types_map = Ident::new(MOD_NAME_WITH_FOREIGN_TYPES, Span::call_site());

    let mut types_map_span: Option<Span> = None;

    let mut ret = TypeMap {
        conv_graph: TypesConvGraph::new(),
        foreign_names_map: HashMap::new(),
        rust_names_map: HashMap::new(),
        utils_code: Vec::with_capacity(file.items.len()),
        generic_edges: Vec::<GenericTypeConv>::new(),
        rust_to_foreign_cache: HashMap::new(),
        //foreign_classes: Vec::new(),
        //exported_enums: HashMap::new(),
        traits_usage_code,
    };

    macro_rules! handle_attrs {
        ($attrs:expr) => {{
            if is_wrong_cfg_pointer_width(&$attrs, target_pointer_width) {
                continue;
            }
            let my_attrs = my_syn_attrs_to_hashmap(&$attrs)?;
            my_attrs
        }};
    }

    fn item_impl_path_is(item_impl: &syn::ItemImpl, var1: &str, var2: &str) -> bool {
        if let syn::ItemImpl {
            trait_: Some((_, ref trait_path, _)),
            ..
        } = item_impl
        {
            is_ident_ignore_params(trait_path, var1) || is_ident_ignore_params(trait_path, var2)
        } else {
            false
        }
    }

    for item in file.items {
        match item {
            Item::Mod(ref item_mod) if item_mod.ident == sym_foreign_types_map => {
                if let Some(span) = types_map_span {
                    let mut err = DiagnosticError::new(
                        item_mod.span(),
                        format!(
                            "Should only one {} per types map",
                            MOD_NAME_WITH_FOREIGN_TYPES
                        ),
                    );
                    err.span_note(span, "Previously defined here");
                    return Err(err);
                }
                types_map_span = Some(item_mod.span());
                debug!("Found foreign_types_map_mod");

                fill_foreign_types_map(item_mod, &mut ret)?;
            }
            Item::Impl(ref item_impl)
                if item_impl_path_is(item_impl, SWIG_INTO_TRAIT, SWIG_FROM_TRAIT) =>
            {
                let swig_attrs = handle_attrs!(item_impl.attrs);
                handle_into_from_impl(&swig_attrs, item_impl, &mut ret)?;
            }
            syn::Item::Trait(item_trait) => {
                let swig_attrs = handle_attrs!(item_trait.attrs);

                if !swig_attrs.is_empty() {
                    let conv_code_template =
                        get_swig_code_from_attrs(item_trait.span(), SWIG_CODE, &swig_attrs)?;

                    ret.traits_usage_code
                        .insert(item_trait.ident.clone(), conv_code_template.to_string());
                }
                ret.utils_code.push(item_trait.into_token_stream());
            }
            Item::Impl(ref item_impl)
                if item_impl_path_is(item_impl, SWIG_DEREF_TRAIT, SWIG_DEREF_MUT_TRAIT) =>
            {
                let swig_attrs = handle_attrs!(item_impl.attrs);
                handle_deref_impl(&swig_attrs, item_impl, &mut ret)?;
            }
            Item::Macro(item_macro) => {
                let swig_attrs = handle_attrs!(item_macro.attrs);
                if swig_attrs.is_empty() {
                    ret.utils_code.push(item_macro.into_token_stream());
                } else {
                    handle_macro(&swig_attrs, item_macro, &mut ret)?;
                }
            }
            _ => {
                ret.utils_code.push(item.into_token_stream());
            }
        }
    }
    Ok(ret)
}

fn fill_foreign_types_map(item_mod: &syn::ItemMod, ret: &mut TypeMap) -> Result<()> {
    let names_map = parse_foreign_types_map_mod(item_mod)?;
    trace!("names_map {:?}", names_map);
    for entry in names_map {
        let TypeNamesMapEntry {
            foreign_name,
            rust_name,
            rust_ty,
        } = entry;
        let rust_name = rust_name.to_string();
        let rust_names_map = &mut ret.rust_names_map;
        let conv_graph = &mut ret.conv_graph;
        let graph_id = *rust_names_map
            .entry(rust_name.clone())
            .or_insert_with(|| conv_graph.add_node(RustType::new(rust_ty, rust_name)));
        let foreign_name = foreign_name.to_string();
        assert!(!ret.foreign_names_map.contains_key(&foreign_name));
        ret.foreign_names_map.insert(foreign_name, graph_id);
    }
    Ok(())
}

#[derive(Debug)]
struct TypeNamesMapEntry {
    foreign_name: Ident,
    rust_name: Ident,
    rust_ty: Type,
}

fn parse_foreign_types_map_mod(item: &ItemMod) -> Result<Vec<TypeNamesMapEntry>> {
    let mut ftype: Option<Ident> = None;

    let mut names_map = HashMap::<Ident, (Ident, Type)>::new();

    for a in &item.attrs {
        if a.path.is_ident(SWIG_FOREIGNER_TYPE) {
            let meta_attr = a.parse_meta()?;
            if let syn::Meta::NameValue(syn::MetaNameValue {
                lit: syn::Lit::Str(value),
                ..
            }) = meta_attr
            {
                ftype = Some(Ident::new(&value.value(), value.span()));
            } else {
                return Err(DiagnosticError::new(
                    meta_attr.span(),
                    "Expect name value attribute",
                ));
            }
        } else if a.path.is_ident(SWIG_RUST_TYPE) {
            let meta_attr = a.parse_meta()?;
            if let Some(ftype) = ftype.take() {
                let attr_value = if let syn::Meta::NameValue(syn::MetaNameValue {
                    lit: syn::Lit::Str(value),
                    ..
                }) = meta_attr
                {
                    value
                } else {
                    return Err(DiagnosticError::new(
                        meta_attr.span(),
                        "Expect name value attribute",
                    ));
                };
                let span = attr_value.span();
                let attr_value_ident = Ident::new(&attr_value.value(), span);

                let rust_ty = syn::parse2::<Type>(quote_spanned!(span=> #attr_value_ident))?;
                names_map.insert(ftype, (attr_value_ident, rust_ty));
            } else {
                return Err(DiagnosticError::new(
                    a.span(),
                    format!("No {} for {}", SWIG_FOREIGNER_TYPE, SWIG_RUST_TYPE),
                ));
            }
        } else {
            return Err(DiagnosticError::new(
                a.span(),
                format!("Unexpected attribute: '{:?}'", a),
            ));
        }
    }

    Ok(names_map
        .into_iter()
        .map(|(k, v)| TypeNamesMapEntry {
            foreign_name: k,
            rust_name: v.0,
            rust_ty: v.1,
        })
        .collect())
}

fn is_wrong_cfg_pointer_width(attrs: &[syn::Attribute], target_pointer_width: usize) -> bool {
    for a in attrs {
        if a.path.is_ident("cfg") {
            if let Ok(syn::Meta::List(syn::MetaList { ref nested, .. })) = a.parse_meta() {
                if nested.len() == 1 {
                    if let syn::NestedMeta::Meta(syn::Meta::NameValue(ref name_val)) = nested[0] {
                        if name_val.ident == "target_pointer_width" {
                            let val = name_val.lit.clone().into_token_stream().to_string();
                            let val = if val.starts_with('"') {
                                &val[1..]
                            } else {
                                &val
                            };
                            let val = if val.ends_with('"') {
                                &val[..val.len() - 1]
                            } else {
                                &val
                            };
                            if let Ok(width) = <usize>::from_str(val) {
                                return target_pointer_width != width;
                            }
                        }
                    }
                }
            }
        }
    }

    false
}

fn my_syn_attrs_to_hashmap(attrs: &[syn::Attribute]) -> Result<MyAttrs> {
    static KNOWN_SWIG_ATTRS: [&str; 6] = [
        SWIG_TO_FOREIGNER_HINT,
        SWIG_FROM_FOREIGNER_HINT,
        SWIG_CODE,
        SWIG_GENERIC_ARG,
        SWIG_FROM_ATTR_NAME,
        SWIG_TO_ATTR_NAME,
    ];
    let mut ret = HashMap::new();
    for a in attrs {
        if KNOWN_SWIG_ATTRS.iter().any(|x| a.path.is_ident(x)) {
            let meta = a.parse_meta()?;
            if let syn::Meta::NameValue(syn::MetaNameValue {
                ref ident,
                lit: syn::Lit::Str(ref value),
                ..
            }) = meta
            {
                ret.entry(ident.to_string())
                    .or_insert_with(Vec::new)
                    .push((value.value(), a.span()));
            } else {
                return Err(DiagnosticError::new(a.span(), "Invalid attribute"));
            }
        }
    }
    Ok(ret)
}

fn get_swig_code_from_attrs<'a, 'b>(
    item_span: Span,
    swig_code_attr_name: &'a str,
    attrs: &'b MyAttrs,
) -> Result<&'b str> {
    if let Some(swig_code) = attrs.get(swig_code_attr_name) {
        if swig_code.len() != 1 {
            Err(DiagnosticError::new(
                item_span,
                format!(
                    "Expect to have {} attribute, and it should be only one",
                    swig_code_attr_name
                ),
            ))
        } else {
            let (ref conv_code_template, sp) = swig_code[0];
            validate_code_template(sp, &conv_code_template.as_str())?;
            Ok(conv_code_template)
        }
    } else {
        Err(DiagnosticError::new(
            item_span,
            format!("No {} attribute", swig_code_attr_name),
        ))
    }
}

fn handle_into_from_impl(
    swig_attrs: &MyAttrs,
    item_impl: &syn::ItemImpl,
    ret: &mut TypeMap,
) -> Result<()> {
    let to_suffix = if !swig_attrs.is_empty() && swig_attrs.contains_key(SWIG_TO_FOREIGNER_HINT) {
        if swig_attrs.len() != 1 || swig_attrs[SWIG_TO_FOREIGNER_HINT].len() != 1 {
            return Err(DiagnosticError::new(
                item_impl.span(),
                format!("Expect only {} attribute", SWIG_TO_FOREIGNER_HINT),
            ));
        }
        Some(swig_attrs[SWIG_TO_FOREIGNER_HINT][0].0.clone())
    } else {
        None
    };

    let from_suffix = if !swig_attrs.is_empty() && swig_attrs.contains_key(SWIG_FROM_FOREIGNER_HINT)
    {
        if swig_attrs.len() != 1 || swig_attrs[SWIG_FROM_FOREIGNER_HINT].len() != 1 {
            return Err(DiagnosticError::new(
                item_impl.span(),
                format!("Expect only {} attribute", SWIG_FROM_FOREIGNER_HINT),
            ));
        }
        Some(swig_attrs[SWIG_FROM_FOREIGNER_HINT][0].0.clone())
    } else {
        None
    };
    let trait_path = if let Some((_, ref trait_path, _)) = item_impl.trait_ {
        trait_path
    } else {
        unreachable!();
    };
    let type_param = extract_trait_param_type(trait_path)?;

    let (from_ty, to_ty, trait_name) = if is_ident_ignore_params(trait_path, SWIG_INTO_TRAIT) {
        (
            (*item_impl.self_ty).clone(),
            type_param.clone(),
            SWIG_INTO_TRAIT,
        )
    } else {
        (
            type_param.clone(),
            (*item_impl.self_ty).clone(),
            SWIG_FROM_TRAIT,
        )
    };

    let conv_code = ret
        .traits_usage_code
        .get(&Ident::new(trait_name, Span::call_site()))
        .ok_or_else(|| {
            DiagnosticError::new(
                item_impl.span(),
                "Can not find conversation code for SwigInto/SwigFrom",
            )
        })?;

    if item_impl.generics.type_params().next().is_some() {
        trace!("handle_into_from_impl: generics {:?}", item_impl.generics);
        let item_code = item_impl.into_token_stream();
        ret.generic_edges.push(GenericTypeConv {
            from_ty,
            to_ty,
            code_template: conv_code.to_string(),
            dependency: Rc::new(RefCell::new(Some(item_code))),
            generic_params: item_impl.generics.clone(),
            to_foreigner_hint: get_foreigner_hint_for_generic(
                &item_impl.generics,
                &swig_attrs,
                ForeignHintVariant::To,
            )?,
            from_foreigner_hint: get_foreigner_hint_for_generic(
                &item_impl.generics,
                &swig_attrs,
                ForeignHintVariant::From,
            )?,
        });
    } else {
        let item_code = item_impl.into_token_stream();
        add_conv_code(
            (from_ty, from_suffix),
            (to_ty, to_suffix),
            item_code,
            conv_code.clone(),
            ret,
        );
    }
    Ok(())
}

fn handle_deref_impl(
    swig_attrs: &MyAttrs,
    item_impl: &syn::ItemImpl,
    ret: &mut TypeMap,
) -> Result<()> {
    let target_ty = unpack_first_associated_type(&item_impl.items, TARGET_ASSOC_TYPE)
        .ok_or_else(|| DiagnosticError::new(item_impl.span(), "No Target associated type"))?;
    debug!(
        "parsing swigderef target {:?}, for_type {:?}",
        target_ty, item_impl.self_ty
    );

    let deref_target_name = normalize_ty_lifetimes(target_ty);
    let trait_path = if let Some((_, ref trait_path, _)) = item_impl.trait_ {
        trait_path
    } else {
        unreachable!();
    };
    let (deref_trait, to_ref_ty) = if is_ident_ignore_params(trait_path, SWIG_DEREF_TRAIT) {
        (
            SWIG_DEREF_TRAIT,
            syn::parse2::<Type>(quote_spanned!(item_impl.span() =>
                                   & #target_ty))?,
        )
    } else {
        (
            SWIG_DEREF_MUT_TRAIT,
            syn::parse2::<Type>(quote_spanned!(item_impl.span() =>
                                   & mut #target_ty))?,
        )
    };

    let conv_code = ret
        .traits_usage_code
        .get(&Ident::new(deref_trait, Span::call_site()))
        .ok_or_else(|| {
            DiagnosticError::new(
                item_impl.span(),
                "Can not find conversation code for SwigDeref/SwigDerefMut",
            )
        })?;
    let from_ty = (*item_impl.self_ty).clone();
    let item_code = item_impl.into_token_stream();
    //for_type -> &Target
    if item_impl.generics.type_params().next().is_some() {
        ret.generic_edges.push(GenericTypeConv {
            from_ty,
            to_ty: to_ref_ty,
            code_template: conv_code.to_string(),
            dependency: Rc::new(RefCell::new(Some(item_code))),
            generic_params: item_impl.generics.clone(),
            to_foreigner_hint: get_foreigner_hint_for_generic(
                &item_impl.generics,
                &swig_attrs,
                ForeignHintVariant::To,
            )?,
            from_foreigner_hint: get_foreigner_hint_for_generic(
                &item_impl.generics,
                &swig_attrs,
                ForeignHintVariant::From,
            )?,
        });
    } else {
        let to_typename = normalize_ty_lifetimes(&to_ref_ty);
        let to_ty = if let Some(ty_type_idx) = ret.rust_names_map.get(&to_typename) {
            ret.conv_graph[*ty_type_idx].ty.clone()
        } else {
            to_ref_ty
        };

        add_conv_code(
            (from_ty, None),
            (to_ty, None),
            item_code,
            conv_code.to_string(),
            ret,
        );
    }
    Ok(())
}

fn handle_macro(swig_attrs: &MyAttrs, item_macro: syn::ItemMacro, ret: &mut TypeMap) -> Result<()> {
    assert!(!swig_attrs.is_empty());

    debug!("conversation macro {:?}", item_macro.ident);

    let from_typename = swig_attrs.get(SWIG_FROM_ATTR_NAME).ok_or_else(|| {
        DiagnosticError::new(
            item_macro.span(),
            format!(
                "No {} but there are other attr {:?}",
                SWIG_FROM_ATTR_NAME, swig_attrs
            ),
        )
    })?;

    assert!(!from_typename.is_empty());
    let to_typename = swig_attrs.get(SWIG_TO_ATTR_NAME).ok_or_else(|| {
        DiagnosticError::new(
            item_macro.span(),
            format!(
                "No {} but there are other attr {:?}",
                SWIG_TO_ATTR_NAME, swig_attrs
            ),
        )
    })?;
    assert!(!to_typename.is_empty());

    let code_template = get_swig_code_from_attrs(item_macro.span(), SWIG_CODE, &swig_attrs)?;

    if let Some(generic_types) = swig_attrs.get(SWIG_GENERIC_ARG) {
        assert!(!generic_types.is_empty());
        let mut types_list = Punctuated::<Type, Token![,]>::new();

        fn spanned_str_to_type((name, span): &(String, Span)) -> Result<Type> {
            let ty: Type = syn::parse_str(name)?;

            let ty: Type = syn::parse2(quote_spanned! { *span => #ty })?;
            Ok(ty)
        }

        for g_ty in generic_types {
            types_list.push(spanned_str_to_type(g_ty)?);
        }
        let generic_params: syn::Generics = parse_quote! { <#types_list> };

        let from_ty: Type = spanned_str_to_type(&from_typename[0])?;
        let to_ty: Type = spanned_str_to_type(&to_typename[0])?;

        let to_foreigner_hint =
            get_foreigner_hint_for_generic(&generic_params, &swig_attrs, ForeignHintVariant::To)?;
        let from_foreigner_hint =
            get_foreigner_hint_for_generic(&generic_params, &swig_attrs, ForeignHintVariant::From)?;

        let item_code = item_macro.into_token_stream();

        ret.generic_edges.push(GenericTypeConv {
            from_ty,
            to_ty,
            code_template: code_template.to_string(),
            dependency: Rc::new(RefCell::new(Some(item_code))),
            generic_params,
            to_foreigner_hint,
            from_foreigner_hint,
        });
    } else {
        unimplemented!();
    }

    Ok(())
}

fn extract_trait_param_type(trait_path: &syn::Path) -> Result<&Type> {
    if trait_path.segments.len() != 1 {
        return Err(DiagnosticError::new(
            trait_path.span(),
            "Invalid trait path",
        ));
    }
    if let syn::PathArguments::AngleBracketed(syn::AngleBracketedGenericArguments {
        ref args,
        ..
    }) = trait_path.segments[0].arguments
    {
        if args.len() != 1 {
            return Err(DiagnosticError::new(
                args.span(),
                "Should be only one generic argument",
            ));
        }
        if let syn::GenericArgument::Type(ref ty) = args[0] {
            Ok(ty)
        } else {
            Err(DiagnosticError::new(args[0].span(), "Expect type here"))
        }
    } else {
        Err(DiagnosticError::new(
            trait_path.segments[0].arguments.span(),
            "Expect generic arguments here",
        ))
    }
}

#[derive(PartialEq, Clone, Copy)]
enum ForeignHintVariant {
    From,
    To,
}

fn get_foreigner_hint_for_generic(
    generic: &syn::Generics,
    attrs: &MyAttrs,
    variant: ForeignHintVariant,
) -> Result<Option<String>> {
    let attr_name = if variant == ForeignHintVariant::To {
        SWIG_TO_FOREIGNER_HINT
    } else {
        SWIG_FROM_FOREIGNER_HINT
    };

    if let Some(attrs) = attrs.get(attr_name) {
        assert!(!attrs.is_empty());
        if attrs.len() != 1 {
            let mut err =
                DiagnosticError::new(attrs[1].1, format!("Several {} attributes", attr_name));
            err.span_note(attrs[0].1, &format!("First {}", attr_name));
            return Err(err);
        }
        let mut ty_params = generic.type_params();
        let first_ty_param = ty_params.next();
        if first_ty_param.is_none() || ty_params.next().is_some() {
            return Err(DiagnosticError::new(
                generic.span(),
                format!("Expect exactly one generic parameter for {}", attr_name),
            ));
        }
        let first_ty_param = first_ty_param.expect("should have value");

        if !attrs[0]
            .0
            .as_str()
            .contains(first_ty_param.ident.to_string().as_str())
        {
            let mut err = DiagnosticError::new(
                attrs[0].1,
                format!("{} not contains {}", attr_name, first_ty_param.ident),
            );
            err.span_note(
                generic.span(),
                format!("{} defined here", first_ty_param.ident),
            );
            return Err(err);
        }
        Ok(Some(attrs[0].0.clone()))
    } else {
        Ok(None)
    }
}

fn add_conv_code(
    from: (Type, Option<String>),
    to: (Type, Option<String>),
    item_code: TokenStream,
    conv_code: String,
    ret: &mut TypeMap,
) {
    let (from, from_suffix) = from;
    let from_typename =
        make_unique_rust_typename_if_need(normalize_ty_lifetimes(&from), from_suffix);
    let from: RustType = RustType::new(from, from_typename);
    let (to, to_suffix) = to;
    let to_typename = make_unique_rust_typename_if_need(normalize_ty_lifetimes(&to), to_suffix);
    let to = RustType::new(to, to_typename);
    debug!(
        "add_conv_code from {} to {}",
        from.normalized_name, to.normalized_name
    );
    let rust_names_map = &mut ret.rust_names_map;
    let conv_graph = &mut ret.conv_graph;
    let from = *rust_names_map
        .entry(from.normalized_name.clone())
        .or_insert_with(|| conv_graph.add_node(from));

    let to = *rust_names_map
        .entry(to.normalized_name.clone())
        .or_insert_with(|| conv_graph.add_node(to));
    conv_graph.add_edge(from, to, TypeConvEdge::new(conv_code, Some(item_code)));
}

fn unpack_first_associated_type<'a, 'b>(
    items: &'a [syn::ImplItem],
    assoc_type_name: &'b str,
) -> Option<&'a Type> {
    for item in items {
        if let syn::ImplItem::Type(ref impl_item_type) = item {
            if impl_item_type.ident == assoc_type_name {
                return Some(&impl_item_type.ty);
            }
        }
    }
    None
}

fn is_ident_ignore_params<I>(path: &syn::Path, ident: I) -> bool
where
    syn::Ident: PartialEq<I>,
{
    path.leading_colon.is_none()
        && path.segments.len() == 1
//        && path.segments[0].arguments.is_none()
        && path.segments[0].ident == ident
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    use quote::quote;
    use syn::parse_quote;

    #[test]
    fn test_parsing_only_types_map_mod() {
        let _ = env_logger::try_init();
        let types_map = parse(
            "foreign_mod",
            r#"
mod swig_foreign_types_map {
    #![swig_foreigner_type="boolean"]
    #![swig_rust_type="jboolean"]
    #![swig_foreigner_type="int"]
    #![swig_rust_type="jint"]
}
"#,
            64,
            HashMap::new(),
        );

        assert_eq!(
            {
                let mut set = HashSet::new();
                set.insert(("boolean".to_string(), "jboolean"));
                set.insert(("int".to_string(), "jint"));
                set
            },
            {
                let mut set = HashSet::new();
                for (k, v) in types_map.foreign_names_map {
                    set.insert((k, types_map.conv_graph[v].normalized_name.as_str()));
                }
                set
            }
        );
    }

    #[test]
    fn test_parse_foreign_types_map_mod() {
        let mut mod_item = syn::parse_str::<ItemMod>(
            r#"
mod swig_foreign_types_map {
    #![swig_foreigner_type="boolean"]
    #![swig_rust_type="jboolean"]
    #![swig_foreigner_type="short"]
    #![swig_rust_type="jshort"]
    #![swig_foreigner_type="int"]
    #![swig_rust_type="jint"]
}
"#,
        )
        .unwrap();
        let map = parse_foreign_types_map_mod(&mod_item).unwrap();
        assert_eq!(
            vec![
                ("boolean".to_string(), "jboolean".to_string()),
                ("int".to_string(), "jint".to_string()),
                ("short".to_string(), "jshort".to_string()),
            ],
            {
                let mut ret = map
                    .into_iter()
                    .map(|v| (v.foreign_name.to_string(), v.rust_name.to_string()))
                    .collect::<Vec<_>>();
                ret.sort_by(|a, b| a.0.cmp(&b.0));
                ret
            }
        );
    }

    #[test]
    fn test_double_map_err() {
        do_parse(
            "double_map_err",
            r#"
mod swig_foreign_types_map {}
mod swig_foreign_types_map {}
"#,
            64,
            HashMap::new(),
        )
        .unwrap_err();
    }

    #[test]
    fn test_parse_cfg_target_width() {
        let _ = env_logger::try_init();
        let item_impl: syn::ItemImpl = parse_quote! {
            #[swig_to_foreigner_hint = "T"]
            #[cfg(target_pointer_width = "64")]
            impl SwigFrom<isize> for jlong {
                fn swig_from(x: isize, _: *mut JNIEnv) -> Self {
                    x as jlong
                }
            }
        };
        assert!(is_wrong_cfg_pointer_width(&item_impl.attrs, 32));
        assert!(!is_wrong_cfg_pointer_width(&item_impl.attrs, 64));
    }

    #[test]
    fn test_my_syn_attrs_to_hashmap() {
        let item_impl: syn::ItemImpl = parse_quote! {
            #[swig_to_foreigner_hint = "T"]
            #[cfg(target_pointer_width = "64")]
            impl SwigFrom<isize> for jlong {
                fn swig_from(x: isize, _: *mut JNIEnv) -> Self {
                    x as jlong
                }
            }
        };
        assert_eq!(
            vec![("swig_to_foreigner_hint".to_string(), vec!["T".to_string()])],
            my_syn_attrs_to_hashmap(&item_impl.attrs)
                .unwrap()
                .into_iter()
                .map(|(k, v)| (k, v.into_iter().map(|v| v.0).collect::<Vec<_>>()))
                .collect::<Vec<_>>()
        );

        let item_impl: syn::ItemImpl = parse_quote! {
            #[swig_to_foreigner_hint = "T"]
            #[swig_code = "let mut {to_var}: {to_var_type} = <{to_var_type}>::swig_from({from_var});"]
            #[cfg(target_pointer_width = "64")]
            impl SwigFrom<isize> for jlong {
                fn swig_from(x: isize, _: *mut JNIEnv) -> Self {
                    x as jlong
                }
            }
        };
        assert_eq!(
            {
                let mut v = vec![
                    ("swig_to_foreigner_hint".to_string(), vec!["T".to_string()]),
                    (
                        "swig_code".to_string(),
                        vec![
                        "let mut {to_var}: {to_var_type} = <{to_var_type}>::swig_from({from_var});"
                            .to_string()
                    ],
                    ),
                ];
                v.sort();
                v
            },
            {
                let mut v: Vec<_> = my_syn_attrs_to_hashmap(&item_impl.attrs)
                    .unwrap()
                    .into_iter()
                    .map(|(k, v)| (k, v.into_iter().map(|v| v.0).collect::<Vec<_>>()))
                    .collect();
                v.sort();
                v
            }
        );
    }

    #[test]
    fn test_extract_trait_param_type() {
        let trait_impl: syn::ItemImpl = parse_quote! {
            impl<'bugaga> SwigFrom<jobject> for Option<&'bugaga str> {
                fn swig_from(x: jobject) -> Self {
                    unimplemented!();
                }
            }
        };
        let trait_impl_path = trait_impl.trait_.unwrap().1;

        assert_eq!(
            {
                let ty: Type = parse_quote!(jobject);
                ty
            },
            *extract_trait_param_type(&trait_impl_path).unwrap()
        );
    }

    #[test]
    fn test_get_foreigner_hint_for_generic() {
        let trait_impl: syn::ItemImpl = parse_quote! {
            #[swig_to_foreigner_hint = "T"]
            impl<T: SwigForeignClass> SwigFrom<T> for *mut ::std::os::raw::c_void {
                fn swig_from(x: T) -> Self {
                    unimplemented!();
                }
            }
        };
        let my_attrs = my_syn_attrs_to_hashmap(&trait_impl.attrs).unwrap();
        assert_eq!(
            "T",
            get_foreigner_hint_for_generic(&trait_impl.generics, &my_attrs, ForeignHintVariant::To)
                .unwrap()
                .unwrap()
        );
    }

    #[test]
    fn test_unpack_first_associated_type() {
        let trait_impl: syn::ItemImpl = parse_quote! {
            impl<T> SwigDeref for Vec<T> {
                type Target = [T];
                fn swig_deref(&self) -> &Self::Target {
                    &*self
                }
            }

        };
        assert_eq!(
            "[ T ]",
            unpack_first_associated_type(&trait_impl.items, "Target")
                .unwrap()
                .into_token_stream()
                .to_string()
        );
    }

    #[test]
    fn test_parse_trait_with_code() {
        let _ = env_logger::try_init();
        let mut conv_map = do_parse(
            "trait_with_code",
            r#"
#[allow(dead_code)]
#[swig_code = "let {to_var}: {to_var_type} = {from_var}.swig_into(env);"]
trait SwigInto<T> {
    fn swig_into(self, env: *mut JNIEnv) -> T;
}

impl SwigInto<bool> for jboolean {
    fn swig_into(self, _: *mut JNIEnv) -> bool {
        self != 0
    }
}

#[allow(dead_code)]
#[swig_code = "let {to_var}: {to_var_type} = <{to_var_type}>::swig_from({from_var}, env);"]
trait SwigFrom<T> {
    fn swig_from(T, env: *mut JNIEnv) -> Self;
}
impl SwigFrom<bool> for jboolean {
    fn swig_from(x: bool, _: *mut JNIEnv) -> Self {
        if x {
            1 as jboolean
        } else {
            0 as jboolean
        }
    }
}
"#,
            64,
            HashMap::new(),
        )
        .unwrap();

        let (_, code) = conv_map
            .convert_rust_types(
                &str_to_rust_ty("jboolean"),
                &str_to_rust_ty("bool"),
                "a0",
                "jlong",
                Span::call_site(),
            )
            .unwrap();
        assert_eq!("    let a0: bool = a0.swig_into(env);\n".to_string(), code);

        let (_, code) = conv_map
            .convert_rust_types(
                &str_to_rust_ty("bool"),
                &str_to_rust_ty("jboolean"),
                "a0",
                "jlong",
                Span::call_site(),
            )
            .unwrap();

        assert_eq!(
            "    let a0: jboolean = <jboolean>::swig_from(a0, env);\n".to_string(),
            code
        );
    }

    #[test]
    fn test_parse_deref() {
        let mut conv_map = do_parse(
            "deref_code",
            r#"
#[allow(dead_code)]
#[swig_code = "let {to_var}: {to_var_type} = {from_var}.swig_deref();"]
trait SwigDeref {
    type Target: ?Sized;
    fn swig_deref(&self) -> &Self::Target;
}

impl SwigDeref for String {
    type Target = str;
    fn swig_deref(&self) -> &str {
        &self
    }
}
"#,
            64,
            HashMap::new(),
        )
        .unwrap();

        let (_, code) = conv_map
            .convert_rust_types(
                &str_to_rust_ty("String"),
                &str_to_rust_ty("&str"),
                "a0",
                "jlong",
                Span::call_site(),
            )
            .unwrap();
        assert_eq!("    let a0: & str = a0.swig_deref();\n".to_string(), code);
    }

    #[test]
    fn test_parse_conv_impl_with_type_params() {
        let mut conv_map = do_parse(
            "trait_with_type_params_code",
            r#"
#[allow(dead_code)]
#[swig_code = "let {to_var}: {to_var_type} = <{to_var_type}>::swig_from({from_var}, env);"]
trait SwigFrom<T> {
    fn swig_from(T, env: *mut JNIEnv) -> Self;
}

#[allow(dead_code)]
#[swig_code = "let {to_var}: {to_var_type} = {from_var}.swig_deref();"]
trait SwigDeref {
    type Target: ?Sized;
    fn swig_deref(&self) -> &Self::Target;
}

impl<T: SwigForeignClass> SwigFrom<T> for jobject {
    fn swig_from(x: T, env: *mut JNIEnv) -> Self {
        object_to_jobject(x, <T>::jni_class_name(), env)
    }
}

impl<T> SwigDeref for Arc<Mutex<T>> {
    type Target = Mutex<T>;
    fn swig_deref(&self) -> &Mutex<T> {
        &self
    }
}

impl<'a, T> SwigFrom<&'a Mutex<T>> for MutexGuard<'a, T> {
    fn swig_from(m: &'a Mutex<T>, _: *mut JNIEnv) -> MutexGuard<'a, T> {
        m.lock().unwrap()
    }
}

impl<'a, T> SwigDeref for MutexGuard<'a, T> {
    type Target = T;
    fn swig_deref(&self) -> &T {
        &self
    }
}
"#,
            64,
            HashMap::new(),
        )
        .unwrap();

        conv_map.add_type(str_to_rust_ty("Foo").implements("SwigForeignClass"));

        let (_, code) = conv_map
            .convert_rust_types(
                &str_to_rust_ty("Arc<Mutex<Foo>>"),
                &str_to_rust_ty("&Foo"),
                "a0",
                "jlong",
                Span::call_site(),
            )
            .unwrap();
        assert_eq!(
            r#"    let a0: & Mutex < Foo > = a0.swig_deref();
    let a0: MutexGuard < Foo > = <MutexGuard < Foo >>::swig_from(a0, env);
    let a0: & Foo = a0.swig_deref();
"#
            .to_string(),
            code
        );
    }

    #[test]
    fn test_parse_macros_conv() {
        let mut conv_map = do_parse(
            "macros",
            r#"
mod swig_foreign_types_map {
    #![swig_foreigner_type="byte"]
    #![swig_rust_type="jbyte"]
    #![swig_foreigner_type="short"]
    #![swig_rust_type="jshort"]
}

#[swig_code = "let {to_var}: {to_var_type} = <{to_var_type}>::swig_from({from_var}, env);"]
trait SwigFrom<T> {
    fn swig_from(T, env: *mut JNIEnv) -> Self;
}

impl SwigFrom<u8> for jshort {
    fn swig_from(x: u8, _: *mut JNIEnv) -> Self {
        x as jshort
    }
}


#[allow(unused_macros)]
#[swig_generic_arg = "T"]
#[swig_generic_arg = "E"]
#[swig_from = "Result<T, E>"]
#[swig_to = "T"]
#[swig_code = "let {to_var}: {to_var_type} = jni_unpack_return!({from_var}, env);"]
macro_rules! jni_unpack_return {
    ($result_value:expr, $default_value:expr, $env:ident) => {
        {
            let ret = match $result_value {
                Ok(x) => x,
                Err(msg) => {
                    jni_throw_exception($env, &msg);
                    return $default_value;
                }
            };
            ret
        }
    }
}
"#,
            64,
            HashMap::new(),
        )
        .unwrap();

        conv_map.add_type(str_to_rust_ty("Foo"));

        let (_, code) = conv_map
            .convert_rust_types(
                &str_to_rust_ty("Result<Foo, String>"),
                &str_to_rust_ty("Foo"),
                "a0",
                "jlong",
                Span::call_site(),
            )
            .unwrap();
        assert_eq!(
            r#"    let a0: Foo = jni_unpack_return!(a0, env);
"#,
            code
        );

        let (_, code) = conv_map
            .convert_rust_types(
                &str_to_rust_ty("Result<u8, &'static str>"),
                &str_to_rust_ty("jshort"),
                "a0",
                "jlong",
                Span::call_site(),
            )
            .unwrap();
        assert_eq!(
            r#"    let a0: u8 = jni_unpack_return!(a0, env);
    let a0: jshort = <jshort>::swig_from(a0, env);
"#,
            code
        );
    }

    fn str_to_rust_ty(code: &str) -> RustType {
        syn::parse_str::<syn::Type>(code).unwrap().into()
    }
}
