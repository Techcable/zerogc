//! Procedural macros for implementing `GcType`

/*
 * The main macro here is `unsafe_impl_gc`
 */

use std::collections::HashSet;

use proc_macro2::{Ident, TokenStream, TokenTree, Span};
use proc_macro_kwargs::parse::{Syn, NestedList};
use syn::{
    Error, Expr, GenericArgument, GenericParam, Generics, PredicateType,
    Token, Type, TypeParamBound, WhereClause, WherePredicate,
    braced, parse_quote
 };
use syn::parse::{ParseStream};
use syn::spanned::Spanned;
use proc_macro_kwargs::{MacroArg, MacroKeywordArgs};

use quote::{quote, quote_spanned};
use super::zerogc_crate;
use syn::ext::IdentExt;

fn empty_clause() -> WhereClause {
    WhereClause {
        predicates: Default::default(),
        where_token: Default::default()
    }
}

#[derive(Debug, MacroKeywordArgs)]
pub struct MacroInput {
    /// The target type we are implementing
    ///
    /// This has unconstrained use of the parameters defined in `params`
    #[kwarg(rename = "target")]
    target_type: Type,
    /// The generic parameters (both types and lifetimes) that we want to
    /// declare for each implementation
    ///
    /// This must not conflict with our internal generic names ;)
    params: NestedList<GenericParam>,
    /// Custom bounds provided for each
    ///
    /// All of these bounds are optional.
    /// This option can be omitted,
    /// giving the equivalent of `bounds = {}`
    #[kwarg(optional)]
    bounds: CustomBounds,
    /// Requirements on implementing NullTrace
    ///
    /// This is unsafe, and completely unchecked.
    null_trace: TraitRequirements,
    /// The associated type implemented as `GcRebrand::Branded`
    #[kwarg(optional)]
    branded_type: Option<Type>,
    /// A (constant) expression determining whether the array needs to be traced
    #[kwarg(rename = "NEEDS_TRACE")]
    needs_trace: Expr,
    /// A (constant) expression determining whether the type should be dropped
    #[kwarg(rename = "NEEDS_DROP")]
    needs_drop: Expr,
    /// The fixed id of the collector, or `None` if the type can work with any collector
    #[kwarg(optional)]
    collector_id: Option<Type>,
    /// An override for the standard `visit_inside_gc` impl.
    ///
    /// This is necessary if the type is not `Sized`
    #[kwarg(optional)]
    visit_inside_gc: Option<VisitInsideGcClosure>,
    /*
     * The following three arguments are parsed
     * into a 'VisitImpl' via the 'parse_visitor' function.
     *
     * TODO: Make the name for this option 'visit_template'
     *
     * I feel like that's a more accurate description
     * of its function.....
     */
    #[kwarg(optional, rename = "visit")]
    raw_visit_template: Option<VisitClosure>,
    #[kwarg(optional, rename = "trace_mut")]
    trace_mut_closure: Option<VisitClosure>,
    #[kwarg(optional, rename = "trace_immutable")]
    trace_immutable_closure: Option<VisitClosure>
}
impl MacroInput {
    fn parse_visitor(&self) -> syn::Result<VisitImpl> {
        if let Some(ref visit_closure) = self.raw_visit_template {
            if let Some(closure) = self.trace_immutable_closure.as_ref()
                .or_else(|| self.trace_mut_closure.as_ref()) {
                return Err(Error::new(
                    closure.0.body.span(),
                    "Cannot specify specific closure (trace_mut/trace_immutable) in addition to `visit`"
                ))
            }
            Ok(VisitImpl::Generic { generic_impl: visit_closure.0.body.clone() })
        } else {
            let trace_closure = self.trace_mut_closure.clone().ok_or_else(|| {
                Error::new(
                    Span::call_site(),
                    "Either a `visit` or a `trace_mut` impl is required for Trace types"
                )
            })?;
            let trace_immut_closure = match self.bounds.trace_immutable {
                Some(TraitRequirements::Never) => {
                    if let Some(ref closure) = self.trace_immutable_closure {
                        return Err(Error::new(
                            closure.0.body.span(),
                            "Specified a `trace_immutable` implementation even though TraceImmutable is never implemented"
                        ))
                    } else {
                        None
                    }
                },
                _ => {
                    let target_span = self.target_type.span();
                    // we maybe implement `TraceImmutable` some of the time
                    Some(self.trace_immutable_closure.clone().ok_or_else(|| {
                        Error::new(
                            target_span,
                            "Requires a `trace_immutable` implementation"
                        )
                    })?)
                }
            };
            Ok(VisitImpl::Specific {
                mutable: ::syn::parse2(trace_closure.0.body)?,
                immutable: trace_immut_closure
                    .map(|closure| ::syn::parse2::<Box<Expr>>(closure.0.body))
                    .transpose()?
            })
        }
    }
    fn basic_generics(&self) -> Generics {
        let mut generics = Generics::default();
        generics.params.extend(self.params.iter().cloned());
        generics
    }
    pub fn expand_output(&self) -> Result<TokenStream, Error> {
        let zerogc_crate = zerogc_crate();
        let target_type = &self.target_type;
        let trace_impl = self.expand_trace_impl(true)?
            .expect("Trace impl required");
        let trace_immutable_impl = self.expand_trace_impl(false)?
            .unwrap_or_default();
        let gcsafe_impl = self.expand_gcsafe_impl();
        let null_trace_clause = match self.null_trace {
            TraitRequirements::Always => Some(empty_clause()),
            TraitRequirements::Where(ref clause) => Some(clause.clone()),
            TraitRequirements::Never => None
        };
        let null_trace_impl = if let Some(null_trace_clause) = null_trace_clause {
            let mut generics = self.basic_generics();
            generics.make_where_clause().predicates.extend(null_trace_clause.predicates);
            let (impl_generics, _, where_clause) = generics.split_for_impl();
            quote! {
                unsafe impl #impl_generics #zerogc_crate::NullTrace for #target_type
                    #where_clause {}
            }
        } else {
            quote!()
        };
        let rebrand_impl = self.expand_rebrand_impl()?;
        Ok(quote! {
            #trace_impl
            #trace_immutable_impl
            #null_trace_impl
            #gcsafe_impl
            #rebrand_impl
        })
    }
    fn expand_trace_impl(&self, mutable: bool) -> Result<Option<TokenStream>, Error> {
        let zerogc_crate = zerogc_crate();
        let target_type = &self.target_type;
        let mut generics = self.basic_generics();
        let clause = if mutable {
            self.bounds.trace_where_clause(&self.params.elements)
        } else {
            match self.bounds.trace_immutable_clause(&self.params.elements) {
                Some(clause) => clause,
                None => return Ok(None), // They are requesting that we dont implement
            }
        };
        generics.make_where_clause().predicates
            .extend(clause.predicates);
        let visit_impl = self.parse_visitor()?.expand_impl(mutable)?;
        let (impl_generics, _, where_clause) = generics.split_for_impl();
        let trait_name = if mutable { quote!(#zerogc_crate::Trace) } else { quote!(#zerogc_crate::TraceImmutable) };
        let visit_method_name = if mutable { quote!(visit) } else { quote!(visit_immutable) };
        let needs_drop_const = if mutable {
            let expr = &self.needs_drop;
            Some(quote!(const NEEDS_DROP: bool = {
                use #zerogc_crate::Trace;
                #expr
            };))
        } else {
            None
        };
        let needs_trace_const = if mutable {
            let expr = &self.needs_trace;
            Some(quote!(const NEEDS_TRACE: bool = {
                // Import the trait so we can access `T::NEEDS_TRACE`
                use #zerogc_crate::Trace;
                #expr
            };))
        } else {
            None
        };
        let visit_inside_gc = if mutable {
            let expr = match self.visit_inside_gc {
                Some(ref expr) => expr.0.body.clone(),
                None => quote!(visitor.visit_gc(gc))
            };
            let where_clause = if let Some(ref clause) = self.bounds.visit_inside_gc {
                clause.clone()
            } else {
                parse_quote!(where Visitor: #zerogc_crate::GcVisitor, ActualId: #zerogc_crate::CollectorId, Self: #zerogc_crate::GcSafe + 'actual_gc)
            };
            Some(quote! {
                #[inline]
                unsafe fn visit_inside_gc<'actual_gc, Visitor, ActualId>(gc: &mut #zerogc_crate::Gc<'actual_gc, Self, ActualId>, visitor: &mut Visitor) -> Result<(), Visitor::Err>
                    #where_clause {
                    #expr
                }
            })
        } else {
            None
        };
        let mutability = if mutable {
            quote!(mut)
        } else {
            quote!()
        };
        Ok(Some(quote! {
            unsafe impl #impl_generics #trait_name for #target_type #where_clause {
                #needs_trace_const
                #needs_drop_const
                #[inline] // TODO: Should this be unconditional?
                fn #visit_method_name<Visitor: #zerogc_crate::GcVisitor + ?Sized>(&#mutability self, visitor: &mut Visitor) -> Result<(), Visitor::Err> {
                    #visit_impl
                }
                #visit_inside_gc
            }
        }))
    }
    fn expand_gcsafe_impl(&self) -> Option<TokenStream> {
        let zerogc_crate = zerogc_crate();
        let target_type = &self.target_type;
        let mut generics = self.basic_generics();
        generics.make_where_clause().predicates
            .extend(match self.bounds.gcsafe_clause(&self.params.elements) {
                Some(clause) => clause.predicates,
                None => return None // They are requesting we dont implement
            });
        let (impl_generics, _, where_clause) = generics.split_for_impl();
        Some(quote! {
            unsafe impl #impl_generics #zerogc_crate::GcSafe for #target_type #where_clause {}
        })
    }
    fn expand_rebrand_impl(&self) -> Result<Option<TokenStream>, Error> {
        let zerogc_crate = zerogc_crate();
        let requirements = self.bounds.rebrand.as_ref();
        let target_type = &self.target_type;
        let mut generics = self.basic_generics();
        let id_type: Type = match self.collector_id {
            Some(ref tp) => tp.clone(),
            None => {
                generics.params.push(parse_quote!(Id: #zerogc_crate::CollectorId));
                parse_quote!(Id)
            }
        };

        let (generate_implicit, default_bounds): (bool, Vec<TypeParamBound>) = match requirements {
            Some(TraitRequirements::Where(ref explicit_requirements)) => {
                generics.make_where_clause().predicates
                    .extend(explicit_requirements.predicates.iter().cloned());
                // they have explicit requirements -> no default bounds
                (false, vec![])
            }
            Some(TraitRequirements::Always) => {
                (false, vec![]) // always should implement (even without implicit bounds)
            },
            Some(TraitRequirements::Never) => {
                return Ok(None); // They are requesting we dont implement it at all
            },
            None => {
                (true, vec![parse_quote!(#zerogc_crate::GcRebrand<'new_gc, #id_type>)])
            }
        };
        // generate default bounds
        for param in &self.params {
            if default_bounds.is_empty() {
                // no defaults to generate
                break
            }
            if !generate_implicit { break } // skip generating implicit bounds
            if let GenericParam::Type(ref tp) = param {
                let type_name = &tp.ident;
                let mut bounds = tp.bounds.clone();
                bounds.extend(default_bounds.iter().cloned());
                generics.make_where_clause()
                    .predicates.push(WherePredicate::Type(PredicateType {
                    lifetimes: None,
                    bounded_ty: self.branded_type.clone().unwrap_or_else(|| {
                        parse_quote!(<#type_name as #zerogc_crate::GcRebrand<'new_gc, Id>>::Branded)
                    }),
                    colon_token: Default::default(),
                    bounds: bounds.clone(),
                }));
                generics.make_where_clause()
                    .predicates.push(WherePredicate::Type(PredicateType {
                    lifetimes: None,
                    bounded_ty: parse_quote!(#type_name),
                    colon_token: Default::default(),
                    bounds
                }))
            }
        }
        if generate_implicit {
            /*
             * If we don't have explicit specification,
             * extend the with the trace clauses
             *
             * TODO: Do we need to apply to the `Branded`/`Erased` types
             */
            generics.make_where_clause().predicates
                .extend(self.bounds.trace_where_clause(&self.params.elements).predicates);
            // Generate `Sized` bounds for all params
            for param in &self.params {
                if let GenericParam::Type(ref tp) = param {
                    let param_name = &tp.ident;
                    generics.make_where_clause().predicates
                        .push(parse_quote!(<#param_name as #zerogc_crate::GcRebrand<'new_gc, Id>>::Branded: Sized))
                }
            }
        }
        generics.params.push(parse_quote!('new_gc));
        let (impl_generics, _, where_clause) = generics.split_for_impl();
        let target_trait = quote!(#zerogc_crate::GcRebrand<'new_gc, #id_type>);
        fn rewrite_brand_trait(
            target: &Type, trait_name: &str, target_params: &HashSet<Ident>,
            target_trait: TokenStream, associated_type: Ident
        ) -> Result<Type, Error> {
            rewrite_type(target, trait_name, &mut |target_type| {
                let ident = match target_type {
                    Type::Path(ref tp) if tp.qself.is_none() => {
                        match tp.path.get_ident() {
                            Some(ident) => ident,
                            None => return None
                        }
                    },
                    _ => return None
                };
                if target_params.contains(ident) {
                    Some(parse_quote!(<#ident as #target_trait>::#associated_type))
                } else {
                    None
                }
            })
        }
        let target_params = self.params.iter().filter_map(|param| match param {
            GenericParam::Type(ref tp) => Some(tp.ident.clone()),
            _ => None
        }).collect::<HashSet<_>>();
        let branded = self.branded_type.clone().map_or_else(|| {
            rewrite_brand_trait(
                &self.target_type, "GcRebrand",
                &target_params,
                parse_quote!(#zerogc_crate::GcRebrand<'new_gc, #id_type>),
                parse_quote!(Branded)
            )
        }, Ok)?;
        Ok(Some(quote! {
            unsafe impl #impl_generics #target_trait for #target_type #where_clause {
                type Branded = #branded;
            }
        }))
    }
}

#[derive(Debug, Clone)]
pub struct KnownArgClosure {
    body: TokenStream,
    brace: ::syn::token::Brace
}
impl KnownArgClosure {
    pub fn parse_with_fixed_args(input: ParseStream, fixed_args: &[&str]) -> syn::Result<Self> {
        let arg_start = input.parse::<Token![|]>()?.span;
        let mut actual_args = Vec::new();
        while !input.peek(Token![|]) {
            // Use 'parse_any' to accept keywords like 'self'
            actual_args.push(Ident::parse_any(input)?);
            if input.peek(Token![|]) {
                break; // done
            } else {
                input.parse::<Token![,]>()?;
            }
        }
        let arg_end = input.parse::<Token![|]>()?.span;
        if actual_args.len() != fixed_args.len() {
            return Err(Error::new(arg_start.join(arg_end).unwrap(), format!(
                "Expected {} args but got {}",
                fixed_args.len(), actual_args.len()
            )));
        }
        for (index, (actual, &expected)) in actual_args.iter().zip(fixed_args).enumerate() {
            if *actual != expected {
                return Err(Error::new(
                    actual.span(),
                    format!("Expected arg #{} to be named {:?}", index, expected)
                ));
            }
        }
        if !input.peek(syn::token::Brace) {
            return Err(input.error("Expected visitor closure to be braced"));
        }
        let body;
        let brace = braced!(body in input);
        let body = body.parse::<TokenStream>()?;
        Ok(KnownArgClosure { body: quote!({ #body }), brace })
    }
}
#[derive(Debug, Clone)]
pub struct VisitClosure(KnownArgClosure);
impl MacroArg for VisitClosure {
    fn parse_macro_arg(input: ParseStream) -> syn::Result<Self> {
        Ok(VisitClosure(KnownArgClosure::parse_with_fixed_args(input, &["self", "visitor"])?))
    }
}
#[derive(Debug)]
pub struct VisitInsideGcClosure(KnownArgClosure);
impl MacroArg for VisitInsideGcClosure {
    fn parse_macro_arg(input: ParseStream) -> syn::Result<Self> {
        Ok(VisitInsideGcClosure(KnownArgClosure::parse_with_fixed_args(input, &["gc", "visitor"])?))
    }
}

/// Extra bounds
#[derive(Default, Debug, MacroKeywordArgs)]
pub struct CustomBounds {
    /// Additional bounds on the `Trace` implementation
    #[kwarg(optional, rename = "Trace")]
    trace: Option<TraitRequirements>,
    /// Additional bounds on the `TraceImmutable` implementation
    ///
    /// If unspecified, this will default to the same as those
    /// specified for `Trace`
    #[kwarg(optional, rename = "TraceImmutable")]
    trace_immutable: Option<TraitRequirements>,
    /// Additional bounds on the `GcSafe` implementation
    ///
    /// If unspecified, this will default to the same as those
    /// specified for `Trace`
    #[kwarg(optional, rename = "GcSafe")]
    gcsafe: Option<TraitRequirements>,
    /// The requirements to implement `GcRebrand`
    #[kwarg(optional, rename = "GcRebrand")]
    rebrand: Option<TraitRequirements>,
    #[kwarg(optional)]
    visit_inside_gc: Option<Syn<WhereClause>>
}
impl CustomBounds {
    fn trace_where_clause(&self, generic_params: &[GenericParam]) -> WhereClause {
        match self.trace {
            Some(TraitRequirements::Never) => unreachable!("Trace must always be implemented"),
            Some(TraitRequirements::Always) => empty_clause(), // No requirements
            Some(TraitRequirements::Where(ref explicit)) => explicit.clone(),
            None => {
                // generate the implicit requiremnents
                let zerogc_crate = zerogc_crate();
                create_clause_with_default(
                    &self.trace, generic_params,
                    vec![parse_quote!(#zerogc_crate::Trace)]
                ).unwrap_or_else(|| unreachable!("Trace must always be implemented"))
            }
        }
    }
    fn trace_immutable_clause(&self, generic_params: &[GenericParam]) -> Option<WhereClause> {
        match self.trace_immutable {
            Some(TraitRequirements::Never) => None, // skip this impl
            Some(TraitRequirements::Always) => Some(empty_clause()), // No requirements
            Some(TraitRequirements::Where(ref explicit)) => Some(explicit.clone()),
            None => {
                let zerogc_crate = zerogc_crate();
                create_clause_with_default(
                    &self.trace_immutable, generic_params,
                    vec![parse_quote!(#zerogc_crate::TraceImmutable)]
                )
            }
        }
    }
    fn gcsafe_clause(&self, generic_params: &[GenericParam]) -> Option<WhereClause> {
        let zerogc_crate = zerogc_crate();
        let mut res = create_clause_with_default(
            &self.gcsafe, generic_params,
            vec![parse_quote!(#zerogc_crate::GcSafe)]
        );
        if self.gcsafe.is_none() {
            // Extend with the trae bounds
            res.get_or_insert_with(empty_clause).predicates.extend(
                self.trace_where_clause(generic_params).predicates
            )
        }
        res
    }
}
fn create_clause_with_default(
    target: &Option<TraitRequirements>, generic_params: &[GenericParam],
    default_bounds: Vec<TypeParamBound>
) -> Option<WhereClause> {
    Some(match *target {
        Some(TraitRequirements::Never) => return None, // do not implement
        Some(TraitRequirements::Where(ref explicit)) => explicit.clone(),
        Some(TraitRequirements::Always) => {
            // Absolutely no conditions on implementation
            empty_clause()
        }
        None => {
            let mut where_clause = empty_clause();
            // Infer bounds for all params
            for param in generic_params {
                if let GenericParam::Type(ref t) = param {
                    let ident = &t.ident;
                    where_clause.predicates.push(WherePredicate::Type(PredicateType {
                        bounded_ty: parse_quote!(#ident),
                        colon_token: Default::default(),
                        bounds: default_bounds.iter().cloned().collect(),
                        lifetimes: None
                    }))
                }
            }
            where_clause
        }
    })
}

/// The visit implementation.
///
/// The target object is always accessible through `self`.
/// Other variables depend on the implementation.
#[derive(Debug)]
pub enum VisitImpl {
    /// A generic implementation, whose code is shared across
    /// both mutable/immutable implementations.
    ///
    /// This requires auto-replacement of certain magic variables,
    /// which vary depending on whether we're generating a mutable
    /// or immutable visitor.
    ///
    /// There are two variables accessible to the implementation: `self` and `visitor`
    ///
    /// | Magic Variable | for Trace  | for TraceImmutable |
    /// | -------------- | ---------- | ------------------ |
    /// | #mutability    | `` (empty) | `mut`              |
    /// | #as_ref        | `as_ref`   | `as_mut`           |
    /// | #iter          | `iter`     | `iter_mut`         |
    /// | #visit_func    | `visit`    | `visit_immutable`  |
    /// | #b             | `&`        | `&mut`             |
    /// | ## (escape)    | `#`        | `#`                |
    ///
    /// Example visitor for `Vec<T>`:
    /// ````no_test
    /// for item in self.#iter() {
    ///     #visit(item);
    /// }
    /// Ok(())
    /// ````
    Generic {
        generic_impl: TokenStream
    },
    /// Specialized implementations which are different for
    /// both `Trace` and `TraceImmutable`
    Specific {
        mutable: Box<Expr>,
        immutable: Option<Box<Expr>>
    }
}
enum MagicVarType {
    Mutability,
    AsRef,
    Iter,
    VisitFunc,
    B
}
impl MagicVarType {
    fn parse_ident(ident: &Ident) -> Result<MagicVarType, Error> {
        let s = ident.to_string();
        Ok(match &*s {
            "mutability" => MagicVarType::Mutability,
            "as_ref" => MagicVarType::AsRef,
            "iter" => MagicVarType::Iter,
            "visit_func" => MagicVarType::VisitFunc,
            "b" => MagicVarType::B,
            _ => return Err(
                Error::new(ident.span(),
                           "Invalid magic variable name"
                ))
        })
    }
}
impl VisitImpl {
    fn expand_impl(&self, mutable: bool) -> Result<Box<Expr>, Error> {
        match *self {
            VisitImpl::Generic { ref generic_impl } => {
                let tokens = replace_magic_tokens(generic_impl.clone(), &mut |ident| {
                    let res = match MagicVarType::parse_ident(ident)? {
                        MagicVarType::Mutability => {
                            if mutable {
                                quote!(mut)
                            } else {
                                quote!()
                            }
                        }
                        MagicVarType::AsRef => {
                            if mutable {
                                quote!(as_mut)
                            } else {
                                quote!(as_ref)
                            }
                        }
                        MagicVarType::Iter => {
                            if mutable {
                                quote!(iter_mut)
                            } else {
                                quote!(iter)
                            }
                        }
                        MagicVarType::VisitFunc => {
                            if mutable {
                                quote!(visit)
                            } else {
                                quote!(visit_immutable)
                            }
                        }
                        MagicVarType::B => {
                            if mutable {
                                quote!(&mut)
                            } else {
                                quote!(&)
                            }
                        }
                    };
                    let span = ident.span(); // Reuse the span of the *input*
                    Ok(quote_spanned!(span => #res))
                })?;
                Ok(match ::syn::parse2::<Box<Expr>>(tokens.clone()) {
                    Ok(res) => res,
                    Err(cause) => {
                        let mut err = Error::new(
                            generic_impl.span(),
                            format!(
                                "Unable to perform 'magic' variable substitution on closure: {}",
                                tokens
                            )
                        );
                        err.combine(cause);
                        return Err(err)
                    }
                })
            }
            VisitImpl::Specific { mutable: ref mutable_impl, ref immutable } => {
                Ok(if mutable {
                    mutable_impl.clone()
                } else {
                    immutable.clone().ok_or_else(|| {
                        Error::new(
                            Span::call_site(),
                            "Expected a trace_immutable closure"
                        )
                    })?
                })
            }
        }
    }
}
fn replace_magic_tokens(input: TokenStream, func: &mut dyn FnMut(&Ident) -> Result<TokenStream, Error>) -> Result<TokenStream, Error> {
    use quote::TokenStreamExt;
    let mut res = TokenStream::new();
    let mut iter = input.into_iter();
    while let Some(item) = iter.next() {
        match item {
            TokenTree::Group(ref group) => {
                let old_span = group.span();
                let delim = group.delimiter();
                let new_stream = replace_magic_tokens(group.stream(), &mut *func)?;
                let mut new_group = ::proc_macro2::Group::new(delim, new_stream);
                new_group.set_span(old_span); // The overall span must be preserved
                res.append(TokenTree::Group(new_group))
            }
            TokenTree::Punct(ref p) if p.as_char() == '#' => {
                match iter.next() {
                    None => return Err(Error::new(
                        p.span(), "Unexpected EOF after magic token `#`"
                    )),
                    Some(TokenTree::Punct(ref p2)) if p2.as_char() == '#' => {
                        // Pass through p2
                        res.append(TokenTree::Punct(p2.clone()));
                    }
                    Some(TokenTree::Ident(ref ident)) => {
                        res.extend(func(ident)?);
                    },
                    Some(ref other) => {
                        return Err(Error::new(
                            p.span(), format!(
                                "Invalid token after magic token `#`: {}",
                                other
                            )
                        ))
                    }
                }
            }
            TokenTree::Punct(_) | TokenTree::Ident(_) | TokenTree::Literal(_)=> {
                // pass through
                res.append(item);
            }
        }
    }
    Ok(res)
}

/// The requirements to implement a trait
///
/// In addition to a where clause, you can specify `always` for unconditional
/// implementation and `never` to forbid generated implementations
#[derive(Clone, Debug)]
pub enum TraitRequirements {
    /// The trait should never be implemented
    Never,
    /// The trait should only be implemented if
    /// the specified where clause is satisfied
    Where(WhereClause),
    /// The trait should always be implemented
    Always
}

impl MacroArg for TraitRequirements {
    fn parse_macro_arg(input: ParseStream) -> syn::Result<Self> {
        if input.peek(syn::Ident) {
            let ident = input.parse::<Ident>()?;
            if ident == "always" {
                Ok(TraitRequirements::Always)
            } else if ident == "never" {
                Ok(TraitRequirements::Never)
            } else {
                Err(Error::new(
                    ident.span(),
                    "Invalid identifier for `TraitRequirement`"
                ))
            }
        } else if input.peek(syn::token::Brace) {
            let inner;
            braced!(inner in input);
            Ok(TraitRequirements::Where(inner.parse::<WhereClause>()?))
        } else {
            Err(input.error("Invalid `TraitRequirement`"))
        }
    }
}



fn rewrite_type(target: &Type, target_type_name: &str, rewriter: &mut dyn FnMut(&Type) -> Option<Type>) -> Result<Type, Error> {
    if let Some(explicitly_rewritten) = rewriter(target) {
        return Ok(explicitly_rewritten)
    }
    let mut target = target.clone();
    match target {
        Type::Paren(ref mut inner) => {
            *inner.elem = rewrite_type(&inner.elem, target_type_name, rewriter)?
        },
        Type::Group(ref mut inner) => {
            *inner.elem = rewrite_type(&inner.elem, target_type_name, rewriter)?
        },
        Type::Reference(ref mut target) => {
            // TODO: Lifetime safety?
            // Rewrite reference target
            *target.elem = rewrite_type(&target.elem, target_type_name, rewriter)?
        }
        Type::Path(::syn::TypePath { ref mut qself, ref mut path }) => {
            *qself = qself.clone().map::<Result<_, Error>, _>(|mut qself| {
                qself.ty = Box::new(rewrite_type(
                    &*qself.ty, target_type_name,
                    &mut *rewriter
                )?);
                Ok(qself)
            }).transpose()?;
            path.segments = path.segments.iter().cloned().map(|mut segment| {
                // old_segment.ident is ignored...
                match segment.arguments {
                    ::syn::PathArguments::None => {}, // Nothing to do here
                    ::syn::PathArguments::AngleBracketed(ref mut generic_args) => {
                        for arg in &mut generic_args.args {
                            match arg {
                                GenericArgument::Lifetime(_) | GenericArgument::Const(_) => {},
                                GenericArgument::Type(ref mut generic_type) => {
                                    *generic_type = rewrite_type(generic_type, target_type_name, &mut *rewriter)?;
                                }
                                GenericArgument::Constraint(_) | GenericArgument::Binding(_) => {
                                    return Err(Error::new(
                                        arg.span(), format!(
                                            "Unable to handle generic arg while rewriting as a {}",
                                            target_type_name
                                        )
                                    ))
                                }
                            }
                        }
                    }
                    ::syn::PathArguments::Parenthesized(ref mut paran_args) => {
                        return Err(Error::new(
                            paran_args.span(),
                            "TODO: Rewriting paranthesized (fn-style) args"
                        ));
                    }
                }
                Ok(segment)
            }).collect::<Result<_, Error>>()?;
        }
        _ => return Err(Error::new(target.span(), format!(
            "Unable to rewrite type as a `{}`: {}",
            target_type_name, quote!(#target)
        )))
    }
    Ok(target)
}
