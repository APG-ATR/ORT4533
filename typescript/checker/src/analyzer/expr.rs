use super::{control_flow::RemoveTypes, export::pat_to_ts_fn_param, Analyzer};
use crate::{
    builtin_types,
    errors::Error,
    ty::{Array, Type, Union},
    util::EqIgnoreSpan,
};
use std::borrow::Cow;
use swc_atoms::js_word;
use swc_common::{Span, Spanned, Visit, VisitWith};
use swc_ecma_ast::*;

impl Analyzer<'_, '_> {
    pub(super) fn type_of<'e>(&'e self, expr: &'e Expr) -> Result<Type<'e>, Error> {
        let span = expr.span();

        Ok(match *expr {
            Expr::This(ThisExpr { span }) => TsType::TsThisType(TsThisType { span }).into(),

            Expr::Ident(Ident {
                sym: js_word!("undefined"),
                ..
            }) => undefined(span),

            Expr::Ident(ref i) => {
                if i.sym == js_word!("require") {
                    unreachable!("typeof(require('...'))");
                }

                if let Some(ty) = self.resolved_imports.get(&i.sym) {
                    return Ok(**ty);
                }

                if let Some(ty) = self.find_var_type(&i.sym) {
                    return Ok(ty);
                }

                if let Some(ty) = builtin_types::get(self.libs, &i.sym) {
                    return Ok(ty);
                }

                // unimplemented!(
                //     "typeof(undefined ident: {})\nFile: {}",
                //     i.sym,
                //     self.path.display()
                // )
                return Err(Error::UndefinedSymbol { span: i.span });
            }

            Expr::Array(ArrayLit { ref elems, .. }) => {
                let mut types: Vec<Type> = vec![];

                for elem in elems {
                    match elem {
                        Some(ExprOrSpread {
                            spread: None,
                            ref expr,
                        }) => {
                            let ty = self.type_of(expr)?.generalize_lit();
                            if types.iter().all(|l| !l.eq_ignore_span(&ty)) {
                                types.push(ty.into_owned())
                            }
                        }
                        Some(ExprOrSpread {
                            spread: Some(..), ..
                        }) => unimplemented!("type of array spread"),
                        None => {
                            let ty = undefined(span);
                            if types.iter().all(|l| !l.eq_ignore_span(&ty)) {
                                types.push(ty)
                            }
                        }
                    }
                }

                Type::Array(Array {
                    span,
                    elem_type: match types.len() {
                        0 => box any(span),
                        1 => box types.into_iter().next().unwrap(),
                        _ => box Union { span, types }.into(),
                    },
                })
            }

            Expr::Lit(Lit::Bool(v)) => TsType::TsLitType(TsLitType {
                span: v.span,
                lit: TsLit::Bool(v),
            })
            .into(),
            Expr::Lit(Lit::Str(ref v)) => TsType::TsLitType(TsLitType {
                span: v.span,
                lit: TsLit::Str(v.clone()),
            })
            .into(),
            Expr::Lit(Lit::Num(v)) => TsType::TsLitType(TsLitType {
                span: v.span,
                lit: TsLit::Number(v),
            })
            .into(),
            Expr::Lit(Lit::Null(Null { span })) => TsType::TsKeywordType(TsKeywordType {
                span,
                kind: TsKeywordTypeKind::TsNullKeyword,
            })
            .into(),
            Expr::Lit(Lit::Regex(..)) => TsType::TsTypeRef(TsTypeRef {
                span,
                type_name: TsEntityName::Ident(Ident {
                    span,
                    sym: js_word!("RegExp"),
                    optional: false,
                    type_ann: None,
                }),
                type_params: None,
            })
            .into(),

            Expr::Paren(ParenExpr { ref expr, .. }) => return self.type_of(expr),

            Expr::Tpl(..) => TsType::TsKeywordType(TsKeywordType {
                span,
                kind: TsKeywordTypeKind::TsStringKeyword,
            })
            .into(),

            Expr::Unary(UnaryExpr {
                op: op!("!"),
                ref arg,
                ..
            }) => negate(self.type_of(arg)?),

            Expr::Unary(UnaryExpr {
                op: op!("typeof"), ..
            }) => TsType::TsKeywordType(TsKeywordType {
                span,
                kind: TsKeywordTypeKind::TsStringKeyword,
            })
            .into(),

            Expr::TsAs(TsAsExpr { ref type_ann, .. }) => (&**type_ann).into(),
            Expr::TsTypeCast(TsTypeCastExpr { ref type_ann, .. }) => (&*type_ann.type_ann).into(),

            Expr::TsNonNull(TsNonNullExpr { ref expr, .. }) => {
                return self.type_of(expr).map(|ty| {
                    // TODO: Optimize

                    ty.into_owned().remove_falsy()
                });
            }

            Expr::Object(ObjectLit { span, ref props }) => TsType::TsTypeLit(TsTypeLit {
                span,
                members: props
                    .iter()
                    .map(|prop| match *prop {
                        PropOrSpread::Prop(ref prop) => self.type_of_prop(&prop),
                        PropOrSpread::Spread(..) => {
                            unimplemented!("spread element in object literal")
                        }
                    })
                    .collect(),
            })
            .into(),

            // https://github.com/Microsoft/TypeScript/issues/26959
            Expr::Yield(..) => any(span),

            Expr::Update(..) => TsType::TsKeywordType(TsKeywordType {
                kind: TsKeywordTypeKind::TsNumberKeyword,
                span,
            })
            .into(),

            Expr::Cond(CondExpr {
                ref cons, ref alt, ..
            }) => {
                let cons_ty = self.type_of(cons)?;
                let alt_ty = self.type_of(alt)?;
                if cons_ty.eq_ignore_span(&alt_ty) {
                    cons_ty
                } else {
                    Union {
                        span,
                        types: vec![cons_ty.into_owned(), alt_ty.into_owned()],
                    }
                    .into()
                }
            }

            Expr::New(NewExpr {
                ref callee,
                ref type_args,
                ref args,
                ..
            }) => {
                let callee_type = self.extract_call_new_expr(
                    callee,
                    ExtractKind::New,
                    args.as_ref().map(|v| &**v).unwrap_or_else(|| &[]),
                    type_args.as_ref(),
                )?;
                return Ok(callee_type);
            }

            Expr::Call(CallExpr {
                callee: ExprOrSuper::Expr(ref callee),
                ref args,
                ref type_args,
                ..
            }) => {
                let callee_type = self
                    .extract_call_new_expr(callee, ExtractKind::Call, args, type_args.as_ref())
                    .map(|v| v.into_owned())?;

                return Ok(callee_type);
            }

            // super() returns any
            Expr::Call(CallExpr {
                callee: ExprOrSuper::Super(..),
                ..
            }) => any(span),

            Expr::Seq(SeqExpr { ref exprs, .. }) => {
                assert!(exprs.len() >= 1);

                return self.type_of(&exprs.last().unwrap());
            }

            Expr::Await(AwaitExpr { .. }) => unimplemented!("typeof(AwaitExpr)"),

            Expr::Class(ClassExpr { ref class, .. }) => return self.type_of_class(class),

            Expr::Arrow(ref e) => return self.type_of_arrow_fn(e),

            Expr::Fn(FnExpr { ref function, .. }) => return self.type_of_fn(&function),

            Expr::Member(MemberExpr {
                obj: ExprOrSuper::Expr(ref obj),
                computed,
                ref prop,
                ..
            }) => {
                match **obj {
                    Expr::Ident(ref i) => {
                        if let Some(Type::Enum(ref e)) = self.scope.find_type(&i.sym) {
                            // TODO(kdy1): Check if variant exists.
                            return Ok(TsType::TsTypeRef(TsTypeRef {
                                span,
                                type_name: TsEntityName::TsQualifiedName(box TsQualifiedName {
                                    left: TsEntityName::Ident(i.clone()),
                                    right: match **prop {
                                        Expr::Ident(ref v) => v.clone(),
                                        _ => unimplemented!(
                                            "error reporting: typeof(non-ident property of \
                                             enum)\nEnum.{:?} ",
                                            prop
                                        ),
                                    },
                                }),
                                type_params: None,
                            })
                            .into());
                        }
                    }
                    _ => {}
                }
                // member expression
                let obj_ty = self
                    .type_of(obj)
                    .map(Box::new)
                    .map(|obj_type| {
                        //
                        Ok(if computed {
                            let index_type = self.type_of(&prop).map(Box::new)?;
                            TsIndexedAccessType {
                                span,
                                obj_type,
                                index_type,
                            }
                        } else {
                            TsIndexedAccessType {
                                span,
                                obj_type,
                                index_type: box TsType::TsKeywordType(TsKeywordType {
                                    span,
                                    kind: TsKeywordTypeKind::TsStringKeyword,
                                }),
                            }
                        })
                    })
                    .map(|res| res.map(TsType::TsIndexedAccessType))??;

                obj_ty.into()
            }

            Expr::MetaProp(..) => unimplemented!("typeof(MetaProp)"),

            Expr::Assign(AssignExpr { ref right, .. }) => return self.type_of(right),

            Expr::Bin(BinExpr {
                op: op!("||"),
                ref right,
                ..
            })
            | Expr::Bin(BinExpr {
                op: op!("&&"),
                ref right,
                ..
            }) => return self.type_of(&right),

            Expr::Bin(BinExpr {
                op: op!(bin, "-"), ..
            }) => TsType::TsKeywordType(TsKeywordType {
                kind: TsKeywordTypeKind::TsNumberKeyword,
                span,
            })
            .into(),

            Expr::Bin(BinExpr { op: op!("==="), .. })
            | Expr::Bin(BinExpr { op: op!("!=="), .. })
            | Expr::Bin(BinExpr { op: op!("!="), .. })
            | Expr::Bin(BinExpr { op: op!("=="), .. })
            | Expr::Bin(BinExpr { op: op!("<="), .. })
            | Expr::Bin(BinExpr { op: op!("<"), .. })
            | Expr::Bin(BinExpr { op: op!(">="), .. })
            | Expr::Bin(BinExpr { op: op!(">"), .. }) => TsType::TsKeywordType(TsKeywordType {
                span,
                kind: TsKeywordTypeKind::TsBooleanKeyword,
            })
            .into(),

            Expr::Unary(UnaryExpr {
                op: op!("void"), ..
            }) => undefined(span),

            _ => unimplemented!("typeof ({:#?})", expr),
        })
    }

    fn type_of_prop(&self, prop: &Prop) -> TsTypeElement {
        TsPropertySignature {
            span: prop.span(),
            key: prop_key_to_expr(&prop),
            params: Default::default(),
            init: None,
            optional: false,
            readonly: false,
            computed: false,
            type_ann: Default::default(),
            type_params: Default::default(),
        }
        .into()
    }

    pub(super) fn type_of_class(&self, c: &Class) -> Result<Type<'static>, Error> {
        let mut type_props = vec![];
        for member in &c.body {
            let span = member.span();
            let any = any(span);

            match member {
                ClassMember::ClassProp(ref p) => {
                    let ty = match p.type_ann.as_ref().map(|ty| Type::from(&*ty.type_ann)) {
                        Some(ty) => ty,
                        None => match p.value {
                            Some(ref e) => self.type_of(&e)?,
                            None => any,
                        },
                    };

                    type_props.push(TsTypeElement::TsPropertySignature(TsPropertySignature {
                        span,
                        key: p.key.clone(),
                        optional: p.is_optional,
                        readonly: p.readonly,
                        init: p.value.clone(),
                        type_ann: Some(TsTypeAnn {
                            span: ty.span(),
                            type_ann: box ty.into_owned(),
                        }),

                        // TODO(kdy1):
                        computed: false,

                        // TODO(kdy1):
                        params: Default::default(),

                        // TODO(kdy1):
                        type_params: Default::default(),
                    }));
                }

                // TODO(kdy1):
                ClassMember::Constructor(ref c) => {
                    type_props.push(TsTypeElement::TsConstructSignatureDecl(
                        TsConstructSignatureDecl {
                            span,

                            // TODO(kdy1):
                            type_ann: None,

                            params: c
                                .params
                                .iter()
                                .map(|param| match *param {
                                    PatOrTsParamProp::Pat(ref pat) => {
                                        pat_to_ts_fn_param(pat.clone())
                                    }
                                    PatOrTsParamProp::TsParamProp(ref prop) => match prop.param {
                                        TsParamPropParam::Ident(ref i) => {
                                            TsFnParam::Ident(i.clone())
                                        }
                                        TsParamPropParam::Assign(AssignPat {
                                            ref left, ..
                                        }) => pat_to_ts_fn_param(*left.clone()),
                                    },
                                })
                                .collect(),

                            // TODO(kdy1):
                            type_params: Default::default(),
                        },
                    ));
                }

                // TODO(kdy1):
                ClassMember::Method(..) => {}

                // TODO(kdy1):
                ClassMember::TsIndexSignature(..) => {}

                ClassMember::PrivateMethod(..) | ClassMember::PrivateProp(..) => {}
            }
        }

        Ok(TsType::TsTypeLit(TsTypeLit {
            span: c.span(),
            members: type_props,
        })
        .into())
    }

    pub(super) fn infer_return_type(
        &self,
        body: &BlockStmt,
    ) -> Result<Option<Type<'static>>, Error> {
        let mut types = vec![];

        struct Visitor<'a> {
            a: &'a Analyzer<'a, 'a>,
            span: Span,
            types: &'a mut Vec<Result<Type<'static>, Error>>,
        }

        impl Visit<ReturnStmt> for Visitor<'_> {
            fn visit(&mut self, stmt: &ReturnStmt) {
                let ty = match stmt.arg {
                    Some(ref arg) => self.a.type_of(arg),
                    None => Ok(undefined(self.span).into()),
                };
                self.types.push(ty.map(|ty| ty.into_owned()));
            }
        }
        let types_len = types.len();
        let types = {
            let mut v = Visitor {
                span: body.span(),
                types: &mut types,
                a: self,
            };
            body.visit_with(&mut v);
            types
        };

        let mut tys = Vec::with_capacity(types_len);
        for ty in types {
            let ty = ty?;
            tys.push(ty);
        }

        match tys.len() {
            0 => Ok(None),
            1 => Ok(Some(tys.into_iter().next().unwrap())),
            _ => Ok(Some(Type::Union(Union {
                span: body.span(),
                types: tys,
            }))
            .map(Type::from)),
        }
    }

    pub(super) fn type_of_arrow_fn(&self, f: &ArrowExpr) -> Result<Type<'static>, Error> {
        let ret_ty = match f.return_type {
            Some(ref ret_ty) => self.expand(f.span, Type::from(&*ret_ty.type_ann))?,
            None => match f.body {
                BlockStmtOrExpr::BlockStmt(ref body) => match self.infer_return_type(body) {
                    Ok(Some(ty)) => ty,
                    Ok(None) => undefined(body.span()),
                    Err(err) => return Err(err),
                },
                BlockStmtOrExpr::Expr(ref expr) => self.type_of(&expr)?,
            },
        };

        Ok(TsType::TsFnOrConstructorType(
            TsFnOrConstructorType::TsFnType(TsFnType {
                span: f.span,
                params: f.params.iter().cloned().map(pat_to_ts_fn_param).collect(),
                type_params: f.type_params.clone(),
                type_ann: TsTypeAnn {
                    span: ret_ty.span(),
                    type_ann: box ret_ty.into_owned(),
                },
            }),
        ))
        .map(Type::from)
    }

    pub(super) fn type_of_fn(&self, f: &Function) -> Result<Type<'static>, Error> {
        let ret_ty = match f.return_type {
            Some(ref ret_ty) => self.expand(f.span, Type::from(&*ret_ty.type_ann))?,
            None => match f.body {
                Some(ref body) => match self.infer_return_type(body) {
                    Ok(Some(ty)) => ty,
                    Ok(None) => undefined(body.span()),
                    Err(err) => return Err(err),
                },
                None => unreachable!("function without body should have type annotation"),
            },
        };

        Ok(TsType::TsFnOrConstructorType(
            TsFnOrConstructorType::TsFnType(TsFnType {
                span: f.span,
                params: f.params.iter().cloned().map(pat_to_ts_fn_param).collect(),
                type_params: f.type_params.clone(),
                type_ann: TsTypeAnn {
                    span: ret_ty.span(),
                    type_ann: box ret_ty.into_owned(),
                },
            }),
        ))
        .map(Type::from)
    }

    fn extract_call_new_expr<'e>(
        &'e self,
        callee: &'e Expr,
        kind: ExtractKind,
        args: &[ExprOrSpread],
        type_args: Option<&TsTypeParamInstantiation>,
    ) -> Result<Type<'e>, Error> {
        let span = callee.span();

        match *callee {
            Expr::Ident(ref i) if i.sym == js_word!("require") => {
                if let Some(dep) = self.resolved_imports.get(
                    &args
                        .iter()
                        .cloned()
                        .map(|arg| match arg {
                            ExprOrSpread { spread: None, expr } => match *expr {
                                Expr::Lit(Lit::Str(Str { value, .. })) => value.clone(),
                                _ => unimplemented!("dynamic import: require()"),
                            },
                            _ => unimplemented!("error reporting: spread element in require()"),
                        })
                        .next()
                        .unwrap(),
                ) {
                    let dep = dep.clone();
                    unimplemented!("dep: {:#?}", dep);
                }

                if let Some(Type::Enum(ref e)) = self.scope.find_type(&i.sym) {
                    return Ok(TsType::TsTypeRef(TsTypeRef {
                        span,
                        type_name: TsEntityName::Ident(i.clone()),
                        type_params: None,
                    })
                    .into());
                }

                Err(Error::UndefinedSymbol { span: i.span() })
            }

            Expr::Member(MemberExpr {
                obj: ExprOrSuper::Expr(ref obj),
                ref prop,
                computed,
                ..
            }) => {
                // member expression
                let obj_type = self.type_of(obj)?;

                match obj_type {
                    Type::Simple(obj_type) => match *obj_type {
                        TsType::TsTypeLit(TsTypeLit { ref members, .. }) => {
                            // Candidates of the method call.
                            //
                            // 4 is just an unsientific guess
                            let mut candidates = Vec::with_capacity(4);

                            for m in members {
                                match m {
                                    TsTypeElement::TsMethodSignature(ref m)
                                        if kind == ExtractKind::Call =>
                                    {
                                        // We are only interested on methods named `prop`
                                        if prop.eq_ignore_span(&m.key) {
                                            candidates.push(m.clone());
                                        }
                                    }

                                    _ => {}
                                }
                            }

                            match candidates.len() {
                                0 => {}
                                1 => {
                                    let TsMethodSignature { type_ann, .. } =
                                        candidates.into_iter().next().unwrap();

                                    return Ok(type_ann
                                        .map(|ty| Type::from(*ty.type_ann))
                                        .unwrap_or_else(|| any(span)));
                                }
                                _ => {
                                    //
                                    for c in candidates {
                                        if c.params.len() == args.len() {
                                            return Ok(c
                                                .type_ann
                                                .map(|ty| Type::from(*ty.type_ann))
                                                .unwrap_or_else(|| any(span)));
                                        }
                                    }

                                    unimplemented!(
                                        "multiple methods with same name and same number of \
                                         arguments"
                                    )
                                }
                            }
                        }

                        TsType::TsKeywordType(TsKeywordType {
                            kind: TsKeywordTypeKind::TsAnyKeyword,
                            ..
                        }) => {
                            return Ok(any(span));
                        }

                        _ => {}
                    },
                }

                if computed {
                    unimplemented!("typeeof(CallExpr): {:?}[{:?}]()", callee, prop)
                } else {
                    Err(if kind == ExtractKind::Call {
                        Error::NoCallSignature { span }
                    } else {
                        Error::NoNewSignature { span }
                    })
                }
            }
            _ => {
                let ty = self.type_of(callee)?;

                self.extract(span, ty, kind, args, type_args)
            }
        }
    }

    fn extract<'a>(
        &'a self,
        span: Span,
        ty: Type<'a>,
        kind: ExtractKind,
        args: &[ExprOrSpread],
        type_args: Option<&TsTypeParamInstantiation>,
    ) -> Result<Type<'a>, Error> {
        let any = any(span);
        let ty = self.expand(span, ty)?;

        macro_rules! ret_err {
            () => {{
                match kind {
                    ExtractKind::Call => return Err(Error::NoCallSignature { span }),
                    ExtractKind::New => return Err(Error::NoNewSignature { span }),
                }
            }};
        }

        match ty {
            Type::Simple(s_ty) => match *s_ty {
                TsType::TsKeywordType(TsKeywordType {
                    kind: TsKeywordTypeKind::TsAnyKeyword,
                    ..
                }) => return Ok(any),

                TsType::TsTypeLit(ref lit) => {
                    for member in &lit.members {
                        match *member {
                            TsTypeElement::TsCallSignatureDecl(TsCallSignatureDecl {
                                ref params,
                                ref type_params,
                                ref type_ann,
                                ..
                            }) if kind == ExtractKind::Call => {
                                //
                                match self.try_instantiate(
                                    span,
                                    ty.span(),
                                    type_ann
                                        .as_ref()
                                        .map(|v| Type::from(&*v.type_ann))
                                        .unwrap_or_else(|| any),
                                    params,
                                    type_params.as_ref(),
                                    args,
                                    type_args,
                                ) {
                                    Ok(v) => return Ok(v),
                                    Err(..) => {}
                                };
                            }

                            TsTypeElement::TsConstructSignatureDecl(TsConstructSignatureDecl {
                                ref params,
                                ref type_params,
                                ref type_ann,
                                ..
                            }) if kind == ExtractKind::New => {
                                match self.try_instantiate(
                                    span,
                                    ty.span(),
                                    type_ann
                                        .as_ref()
                                        .map(|v| Type::from(&*v.type_ann))
                                        .unwrap_or_else(|| any),
                                    params,
                                    type_params.as_ref(),
                                    args,
                                    type_args,
                                ) {
                                    Ok(v) => return Ok(v),
                                    Err(..) => {
                                        // TODO: Handle error
                                    }
                                }
                            }
                            _ => {}
                        }
                    }

                    ret_err!()
                }

                TsType::TsFnOrConstructorType(ref f_c) => match *f_c {
                    TsFnOrConstructorType::TsFnType(TsFnType {
                        ref params,
                        ref type_params,
                        ref type_ann,
                        ..
                    }) if kind == ExtractKind::Call => self.try_instantiate(
                        span,
                        ty.span(),
                        Type::from(&*type_ann.type_ann),
                        params,
                        type_params.as_ref(),
                        args,
                        type_args,
                    ),

                    TsFnOrConstructorType::TsConstructorType(TsConstructorType {
                        ref params,
                        ref type_params,
                        ref type_ann,
                        ..
                    }) if kind == ExtractKind::New => self.try_instantiate(
                        span,
                        ty.span(),
                        Type::from(&*type_ann.type_ann),
                        params,
                        type_params.as_ref(),
                        args,
                        type_args,
                    ),

                    _ => ret_err!(),
                },

                TsType::TsUnionOrIntersectionType(TsUnionOrIntersectionType::TsUnionType(
                    ref u,
                )) => {
                    let mut errors = vec![];
                    for ty in &u.types {
                        match self.extract(span, (&**ty).into(), kind, args, type_args) {
                            Ok(ty) => return Ok(ty),
                            Err(err) => errors.push(err),
                        }
                    }

                    Err(Error::UnionError { span, errors })
                }

                _ => ret_err!(),
            },
        }
    }

    fn try_instantiate<'a>(
        &'a self,
        span: Span,
        callee_span: Span,
        ret_type: Type<'a>,
        param_decls: &[TsFnParam],
        ty_params_decl: Option<&TsTypeParamDecl>,
        args: &[ExprOrSpread],
        i: Option<&TsTypeParamInstantiation>,
    ) -> Result<Type<'a>, Error> {
        {
            // let type_params_len = ty_params_decl.map(|decl|
            // decl.params.len()).unwrap_or(0); let type_args_len = i.map(|v|
            // v.params.len()).unwrap_or(0);

            // // TODO: Handle multiple definitions
            // let min = ty_params_decl
            //     .map(|decl| decl.params.iter().filter(|p|
            // p.default.is_none()).count())
            //     .unwrap_or(type_params_len);

            // let expected = min..=type_params_len;
            // if !expected.contains(&type_args_len) {
            //     return Err(Error::WrongTypeParams {
            //         span,
            //         callee: callee_span,
            //         expected,
            //         actual: type_args_len,
            //     });
            // }
        }

        {
            // TODO: Handle default parameters
            // TODO: Handle multiple definitions

            let min = param_decls
                .iter()
                .filter(|p| match p {
                    TsFnParam::Ident(Ident { optional: true, .. }) => false,
                    _ => true,
                })
                .count();

            let expected = min..=param_decls.len();
            if !expected.contains(&args.len()) {
                return Err(Error::WrongParams {
                    span,
                    callee: callee_span,
                    expected,
                    actual: args.len(),
                });
            }
        }

        Ok(ret_type.into())
    }

    /// Expands
    ///
    ///   - Type alias
    pub(super) fn expand<'t>(&'t self, span: Span, ty: Type<'t>) -> Result<Type<'t>, Error> {
        match ty {
            Type::Simple(s_ty) => match *s_ty {
                TsType::TsTypeRef(TsTypeRef {
                    ref type_name,
                    ref type_params,
                    ..
                }) => {
                    match *type_name {
                        // Check for builtin types
                        TsEntityName::Ident(ref i) => match i.sym {
                            js_word!("Record") => {}
                            js_word!("Readonly") => {}
                            js_word!("ReadonlyArray") => {}
                            js_word!("ReturnType") => {}
                            js_word!("Partial") => {}
                            js_word!("Required") => {}
                            js_word!("NonNullable") => {}
                            js_word!("Pick") => {}
                            js_word!("Record") => {}
                            js_word!("Extract") => {}
                            js_word!("Exclude") => {}

                            _ => {}
                        },
                        _ => {}
                    }

                    let e = (|| {
                        fn root(n: &TsEntityName) -> &Ident {
                            match *n {
                                TsEntityName::TsQualifiedName(box TsQualifiedName {
                                    ref left,
                                    ..
                                }) => root(left),
                                TsEntityName::Ident(ref i) => i,
                            }
                        }

                        // Search imports / decls.
                        let root = root(type_name);

                        if let Some(v) = self.resolved_imports.get(&root.sym) {
                            return Ok(**v);
                        }

                        if let Some(v) = self.scope.find_type(&root.sym) {
                            return Ok(v);
                        }

                        // TODO: Resolve transitive imports.

                        Err(Error::Unimplemented {
                            span: ty.span(),
                            msg: format!(
                                "expand_export_info({})\nFile: {}",
                                root.sym,
                                self.path.display()
                            ),
                        })
                    })()?;

                    return Ok(ty);

                    // match e.extra {
                    //     Some(ref extra) => {
                    //         // Expand
                    //         match extra {

                    //             ExportExtra::Module(TsModuleDecl {
                    //                 body: Some(body), ..
                    //             })
                    //             | ExportExtra::Namespace(TsNamespaceDecl {
                    // box body, .. }) => {                 
                    // let mut name = type_name;            
                    // let mut body = body;                 
                    // let mut ty = None;

                    //                 while let
                    // TsEntityName::TsQualifiedName(q) = name {
                    //                     body = match body {
                    //                         
                    // TsNamespaceBody::TsModuleBlock(ref module) => {
                    //                             match q.left {
                    //                                 TsEntityName::Ident(ref
                    // left) => {                           
                    // for item in module.body.iter() {}
                    //                                     return
                    // Err(Error::UndefinedSymbol {
                    //                                         span: left.span,
                    //                                     });
                    //                                 }
                    //                                 _ => {
                    //                                     //
                    //                                     
                    // unimplemented!("qname")              
                    // }                             }
                    //                         }
                    //                         
                    // TsNamespaceBody::TsNamespaceDecl(TsNamespaceDecl {
                    //                             ref id,
                    //                             ref body,
                    //                             ..
                    //                         }) => {
                    //                             match q.left {
                    //                                 TsEntityName::Ident(ref
                    // left) => {                           
                    // if id.sym != left.sym {              
                    // return Err(Error::UndefinedSymbol {
                    //                                             span:
                    // left.span,                           
                    // });                                  
                    // }                                 }
                    //                                 _ => {}
                    //                             }
                    //                             //
                    //                             body
                    //                         }
                    //                     };
                    //                     name = &q.left;
                    //                 }

                    //                 return match ty {
                    //                     Some(ty) => Ok(ty),
                    //                     None => Err(Error::UndefinedSymbol {
                    // span }),                 };
                    //             }
                    //             ExportExtra::Module(..) => {
                    //                 assert_eq!(*type_params, None);

                    //                 unimplemented!(
                    //                     "ExportExtra::Module without body
                    // cannot be instantiated"              
                    // )             }
                    //             ExportExtra::Interface(ref i) => {
                    //                 // TODO: Check length of type parmaters
                    //                 // TODO: Instantiate type parameters

                    //                 let members =
                    // i.body.body.iter().cloned().collect();

                    //                 return Ok(TsType::TsTypeLit(TsTypeLit {
                    //                     span: i.span,
                    //                     members,
                    //                 })
                    //                 .into());
                    //             }
                    //             ExportExtra::Alias(ref decl) => {
                    //                 // TODO(kdy1): Handle type parameters.
                    //                 return Ok(decl.type_ann.into());
                    //             }
                    //         }
                    //     }
                    //     None => unimplemented!("`ty` and `extra` are both
                    // null"), }
                }

                TsType::TsTypeQuery(TsTypeQuery { ref expr_name, .. }) => match *expr_name {
                    TsEntityName::Ident(ref i) => return self.type_of(&Expr::Ident(i.clone())),
                    _ => unimplemented!("expand(TsTypeQuery): typeof member.expr"),
                },

                _ => {}
            },
        }

        Ok(ty)
    }
}

fn prop_key_to_expr(p: &Prop) -> Box<Expr> {
    match *p {
        Prop::Shorthand(ref i) => box Expr::Ident(i.clone()),
        Prop::Assign(AssignProp { ref key, .. }) => box Expr::Ident(key.clone()),
        Prop::Getter(GetterProp { ref key, .. })
        | Prop::KeyValue(KeyValueProp { ref key, .. })
        | Prop::Method(MethodProp { ref key, .. })
        | Prop::Setter(SetterProp { ref key, .. }) => match *key {
            PropName::Computed(ref expr) => expr.clone(),
            PropName::Ident(ref ident) => box Expr::Ident(ident.clone()),
            PropName::Str(ref s) => box Expr::Lit(Lit::Str(Str { ..s.clone() })),
            PropName::Num(ref s) => box Expr::Lit(Lit::Num(Number { ..s.clone() })),
        },
    }
}

#[inline]
pub(super) fn never_ty(span: Span) -> Type<'static> {
    TsType::TsKeywordType(TsKeywordType {
        span,
        kind: TsKeywordTypeKind::TsNeverKeyword,
    })
    .into()
}

fn negate(ty: Type) -> Type {
    fn boolean(span: Span) -> Type<'static> {
        TsType::TsKeywordType(TsKeywordType {
            span,
            kind: TsKeywordTypeKind::TsBooleanKeyword,
        })
        .into()
    }

    match ty {
        Type::Simple(ref ty) => match **ty {
            TsType::TsLitType(TsLitType { ref lit, span }) => match *lit {
                TsLit::Bool(v) => TsType::TsLitType(TsLitType {
                    lit: TsLit::Bool(Bool {
                        value: !v.value,
                        ..v
                    }),
                    span,
                })
                .into(),
                TsLit::Number(v) => TsType::TsLitType(TsLitType {
                    lit: TsLit::Bool(Bool {
                        value: v.value != 0.0,
                        span: v.span,
                    }),
                    span,
                })
                .into(),
                TsLit::Str(ref v) => TsType::TsLitType(TsLitType {
                    lit: TsLit::Bool(Bool {
                        value: v.value != js_word!(""),
                        span: v.span,
                    }),
                    span,
                })
                .into(),
            },
            _ => boolean(ty.span()),
        },
    }
}

pub const fn undefined(span: Span) -> Type<'static> {
    TsType::TsKeywordType(TsKeywordType {
        span,
        kind: TsKeywordTypeKind::TsUndefinedKeyword,
    })
    .into()
}

pub const fn any(span: Span) -> Type<'static> {
    TsType::TsKeywordType(TsKeywordType {
        span,
        kind: TsKeywordTypeKind::TsAnyKeyword,
    })
    .into()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExtractKind {
    Call,
    New,
}
