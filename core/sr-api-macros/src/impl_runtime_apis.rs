// Copyright 2018 Parity Technologies (UK) Ltd.
// This file is part of Substrate.

// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Substrate.  If not, see <http://www.gnu.org/licenses/>.

use utils::{
	unwrap_or_error, generate_crate_access, generate_hidden_includes,
	generate_runtime_mod_name_for_trait, fold_fn_decl_for_client_side
};

use proc_macro;
use proc_macro2::{Span, TokenStream};

use quote::quote;

use syn::{
	spanned::Spanned, parse_macro_input, Ident, Type, ItemImpl, MethodSig, FnArg, Path,
	ImplItem, parse::{Parse, ParseStream, Result, Error}, PathArguments, GenericArgument, TypePath,
	fold::{self, Fold}, FnDecl, parse_quote, Pat
};

use std::iter;

/// Unique identifier used to make the hidden includes unique for this macro.
const HIDDEN_INCLUDES_ID: &str = "IMPL_RUNTIME_APIS";

/// The structure used for parsing the runtime api implementations.
struct RuntimeApiImpls {
	impls: Vec<ItemImpl>,
}

impl Parse for RuntimeApiImpls {
	fn parse(input: ParseStream) -> Result<Self> {
		let mut impls = Vec::new();

		while !input.is_empty() {
			impls.push(ItemImpl::parse(input)?);
		}

		Ok(Self { impls })
	}
}

/// Generates the call to the implementation of the requested function.
/// The generated code includes decoding of the input arguments and encoding of the output.
fn generate_impl_call(
	signature: &MethodSig,
	runtime: &Type,
	input: &Ident,
	impl_trait: &Path
) -> Result<TokenStream> {
	let mut pnames = Vec::new();
	let mut ptypes = Vec::new();
	let mut generated_pattern_counter = 0;
	for input in signature.decl.inputs.iter() {
		match input {
			FnArg::Captured(arg) => {
				match &arg.ty {
					Type::Reference(_) => {
						return Err(
							Error::new(
								arg.ty.span(),
								"No type references are allowed in the api traits!"
							)
						)
					},
					_ => {},
				}

				pnames.push(
					generate_unique_pattern(arg.pat.clone(), &mut generated_pattern_counter)
				);
				ptypes.push(&arg.ty);
			},
			_ => {
				return Err(
					Error::new(
						input.span(),
						"Only function arguments with the following \
						pattern are accepted: `name: type`!"
					)
				)
			}
		}
	}

	let c = generate_crate_access(HIDDEN_INCLUDES_ID);
	let c_iter = iter::repeat(&c);
	let fn_name = &signature.ident;
	let fn_name_str = iter::repeat(fn_name.to_string());
	let input = iter::repeat(input);
	let pnames2 = pnames.clone();

	Ok(
		quote!(
			#(
				let #pnames : #ptypes = match #c_iter::runtime_api::Decode::decode(&mut #input) {
					Some(input) => input,
					None => panic!("Bad input data provided to {}", #fn_name_str),
				};
			)*

			let output = <#runtime as #impl_trait>::#fn_name(#( #pnames2 ),*);
			#c::runtime_api::Encode::encode(&output)
		).into()
	)
}

/// Extract the trait that is implemented in the given `ItemImpl`.
fn extract_impl_trait<'a>(impl_: &'a ItemImpl) -> Result<&'a Path> {
	impl_.trait_.as_ref().map(|v| &v.1).ok_or_else(
		|| Error::new(impl_.span(), "Only implementation of traits are supported!")
	).and_then(|p| {
		if p.segments.len() > 1 {
			Ok(p)
		} else {
			Err(
				Error::new(
					p.span(),
					"The implemented trait has to be referenced with a path, \
					e.g. `impl client::Core for Runtime`."
				)
			)
		}
	})
}

/// Extracts the runtime block identifier.
fn extract_runtime_block_ident(trait_: &Path) -> Result<&TypePath> {
	let span = trait_.span();
	let segment = trait_
		.segments
		.last()
		.ok_or_else(
			|| Error::new(span, "Empty path not supported")
		)?;
	let generics = segment.value();

	match &generics.arguments {
		PathArguments::AngleBracketed(ref args) => {
			args.args.first().and_then(|v| match v.value() {
			GenericArgument::Type(Type::Path(block)) => Some(block),
				_ => None
			}).ok_or_else(|| Error::new(args.span(), "Missing `Block` generic parameter."))
		},
		PathArguments::None => {
			let span = trait_.segments.last().as_ref().unwrap().value().span();
			Err(Error::new(span, "Missing `Block` generic parameter."))
		},
		PathArguments::Parenthesized(_) => {
			Err(Error::new(generics.arguments.span(), "Unexpected parentheses in path!"))
		}
	}
}

/// Generate all the implementation calls for the given functions.
fn generate_impl_calls(impls: &[ItemImpl], input: &Ident) -> Result<Vec<(Ident, TokenStream)>> {
	let mut impl_calls = Vec::new();

	for impl_ in impls {
		let impl_trait = extend_with_runtime_decl_path(extract_impl_trait(impl_)?.clone());

		for item in &impl_.items {
			match item {
				ImplItem::Method(method) => {
					let impl_call = generate_impl_call(
						&method.sig,
						&impl_.self_ty,
						input,
						&impl_trait
					)?;

					impl_calls.push((method.sig.ident.clone(), impl_call));
				},
				_ => {},
			}
		}
	}

	Ok(impl_calls)
}

/// Generate the dispatch function that is used in native to call into the runtime.
fn generate_dispatch_function(impls: &[ItemImpl]) -> Result<TokenStream> {
	let data = Ident::new("data", Span::call_site());
	let impl_calls = generate_impl_calls(impls, &data)?.into_iter().map(|(fn_name, impl_)| {
		let fn_name = fn_name.to_string();
		quote!( #fn_name => Some({ #impl_ }), )
	});

	Ok(quote!(
		#[cfg(feature = "std")]
		pub fn dispatch(method: &str, mut #data: &[u8]) -> Option<Vec<u8>> {
			match method {
				#( #impl_calls )*
				_ => None,
			}
		}
	).into())
}

/// Generate the interface functions that are used to call into the runtime in wasm.
fn generate_wasm_interface(impls: &[ItemImpl]) -> Result<TokenStream> {
	let input = Ident::new("input", Span::call_site());
	let c = generate_crate_access(HIDDEN_INCLUDES_ID);
	let impl_calls = generate_impl_calls(impls, &input)?.into_iter().map(|(fn_name, impl_)| {
		quote!(
			#[cfg(not(feature = "std"))]
			#[no_mangle]
			pub fn #fn_name(input_data: *mut u8, input_len: usize) -> u64 {
				let mut #input = if input_len == 0 {
					&[0u8; 0]
				} else {
					unsafe {
						#c::runtime_api::slice::from_raw_parts(input_data, input_len)
					}
				};

				let output = { #impl_ };
				let res = output.as_ptr() as u64 + ((output.len() as u64) << 32);

				// Leak the output vector to avoid it being freed.
				// This is fine in a WASM context since the heap
				// will be discarded after the call.
				::core::mem::forget(output);
				res
			}
		)
	});

	Ok(quote!( #( #impl_calls )* ))
}

fn generate_block_and_block_id_ty(
	runtime: &Type,
	trait_: &'static str,
	assoc_type: &'static str,
) -> (TokenStream, TokenStream) {
	let crate_ = generate_crate_access(HIDDEN_INCLUDES_ID);
	let trait_ = Ident::new(trait_, Span::call_site());
	let assoc_type = Ident::new(assoc_type, Span::call_site());

	let block = quote!( <#runtime as #crate_::runtime_api::#trait_>::#assoc_type );
	let block_id = quote!( #crate_::runtime_api::BlockId<#block> );

	(block, block_id)
}

fn generate_node_block_and_block_id_ty(runtime: &Type) -> (TokenStream, TokenStream) {
	generate_block_and_block_id_ty(runtime, "GetNodeBlockType", "NodeBlock")
}

fn generate_runtime_api_base_structures(impls: &[ItemImpl]) -> Result<TokenStream> {
	let crate_ = generate_crate_access(HIDDEN_INCLUDES_ID);
	let runtime = &impls.get(0).ok_or_else(||
		Error::new(Span::call_site(), "No api implementation given!")
	)?.self_ty;
	let (block, block_id) = generate_node_block_and_block_id_ty(runtime);

	Ok(quote!(
		/// Implements all runtime apis for the client side.
		#[cfg(any(feature = "std", test))]
		pub struct RuntimeApi {
			call: ::std::ptr::NonNull<#crate_::runtime_api::CallApiAt<#block>>,
			commit_on_success: ::std::cell::RefCell<bool>,
			initialised_block: ::std::cell::RefCell<Option<#block_id>>,
			changes: ::std::cell::RefCell<#crate_::runtime_api::OverlayedChanges>,
		}

		// `RuntimeApi` itself is not threadsafe. However, an instance is only available in a
		// `ApiRef` object and `ApiRef` also has an associated lifetime. This lifetimes makes it
		// impossible to move `RuntimeApi` into another thread.
		#[cfg(any(feature = "std", test))]
		unsafe impl Send for RuntimeApi {}
		#[cfg(any(feature = "std", test))]
		unsafe impl Sync for RuntimeApi {}

		#[cfg(any(feature = "std", test))]
		impl #crate_::runtime_api::ApiExt for RuntimeApi {
			fn map_api_result<F: FnOnce(&Self) -> ::std::result::Result<R, E>, R, E>(
				&self,
				map_call: F
			) -> ::std::result::Result<R, E> {
				*self.commit_on_success.borrow_mut() = false;
				let res = map_call(self);
				*self.commit_on_success.borrow_mut() = true;

				self.commit_on_ok(&res);

				res
			}
		}

		#[cfg(any(feature = "std", test))]
		impl #crate_::runtime_api::ConstructRuntimeApi<#block> for RuntimeApi {
			fn construct_runtime_api<'a, T: #crate_::runtime_api::CallApiAt<#block>>(
				call: &'a T
			) -> #crate_::runtime_api::ApiRef<'a, Self> {
				RuntimeApi {
					call: unsafe {
						::std::ptr::NonNull::new_unchecked(
							call as &#crate_::runtime_api::CallApiAt<#block> as *const _ as *mut _
						)
					},
					commit_on_success: true.into(),
					initialised_block: None.into(),
					changes: Default::default(),
				}.into()
			}
		}

		#[cfg(any(feature = "std", test))]
		impl RuntimeApi {
			fn call_api_at<A: #crate_::runtime_api::Encode, R: #crate_::runtime_api::Decode>(
				&self,
				at: &#block_id,
				function: &'static str,
				args: &A
			) -> #crate_::error::Result<R> {
				let res = unsafe {
					self.call.as_ref().call_api_at(
						at,
						function,
						args.encode(),
						&mut *self.changes.borrow_mut(),
						&mut *self.initialised_block.borrow_mut()
					).and_then(|r|
						R::decode(&mut &r[..])
							.ok_or_else(||
								#crate_::error::ErrorKind::CallResultDecode(function).into()
							)
					)
				};

				self.commit_on_ok(&res);
				res
			}

			fn commit_on_ok<R, E>(&self, res: &::std::result::Result<R, E>) {
				if *self.commit_on_success.borrow() {
					if res.is_err() {
						self.changes.borrow_mut().discard_prospective();
					} else {
						self.changes.borrow_mut().commit_prospective();
					}
				}
			}
		}
	))
}

/// Extend the given trait path with module that contains the declaration of the trait for the
/// runtime.
fn extend_with_runtime_decl_path(mut trait_: Path) -> Path {
	let runtime = {
		let trait_name = &trait_
			.segments
			.last()
			.as_ref()
			.expect("Trait path should always contain at least one item; qed")
			.value()
			.ident;

		generate_runtime_mod_name_for_trait(trait_name)
	};

	let pos = trait_.segments.len() - 1;
	trait_.segments.insert(pos, runtime.clone().into());
	trait_
}

/// Generates the implementations of the apis for the runtime.
fn generate_api_impl_for_runtime(impls: &[ItemImpl]) -> Result<TokenStream> {
	let mut impls_prepared = Vec::new();

	// We put `runtime` before each trait to get the trait that is intended for the runtime and
	// we put the `RuntimeBlock` as first argument for the trait generics.
	for impl_ in impls.iter() {
		let mut impl_ = impl_.clone();
		let trait_ = extract_impl_trait(&impl_)?.clone();
		let trait_ = extend_with_runtime_decl_path(trait_);

		impl_.trait_.as_mut().unwrap().1 = trait_;
		impls_prepared.push(impl_);
	}

	Ok(quote!( #( #impls_prepared )* ))
}

/// Generate an unique pattern based on the given counter, if the given pattern is a `_`.
fn generate_unique_pattern(pat: Pat, counter: &mut u32) -> Pat {
	match pat {
		Pat::Wild(_) => {
			let generated_name = Ident::new(
				&format!("impl_runtime_api_generated_name_{}", counter),
				pat.span()
			);
			*counter += 1;

			parse_quote!( #generated_name )
		},
		_ => pat,
	}
}

/// Auxilariy data structure that is used to convert `impl Api for Runtime` to
/// `impl Api for RuntimeApi`.
/// This requires us to replace the runtime `Block` with the node `Block`,
/// `impl Api for Runtime` with `impl Api for RuntimeApi` and replace the method implementations
/// with code that calls into the runtime.
struct ApiRuntimeImplToApiRuntimeApiImpl<'a> {
	node_block: &'a TokenStream,
	runtime_block: &'a TypePath,
	node_block_id: &'a TokenStream,
}

impl<'a> Fold for ApiRuntimeImplToApiRuntimeApiImpl<'a> {
	fn fold_type_path(&mut self, input: TypePath) -> TypePath {
		let new_ty_path = if input == *self.runtime_block {
			let node_block = self.node_block;
			parse_quote!( #node_block )
		} else {
			input
		};

		fold::fold_type_path(self, new_ty_path)
	}

	fn fold_fn_decl(&mut self, input: FnDecl) -> FnDecl {
		let input = fold_fn_decl_for_client_side(
			input,
			&self.node_block_id,
			&generate_crate_access(HIDDEN_INCLUDES_ID)
		);

		fold::fold_fn_decl(self, input)
	}

	fn fold_impl_item_method(&mut self, mut input: syn::ImplItemMethod) -> syn::ImplItemMethod {
		{
			let mut generated_name_counter = 0;
			let arg_names = input.sig.decl.inputs.iter_mut().filter_map(|i| match i {
				FnArg::Captured(ref mut arg) => Some(&mut arg.pat),
				_ => None,
			}).map(|p| {
				*p = generate_unique_pattern(p.clone(), &mut generated_name_counter);
				p
			});
			let name = input.sig.ident.to_string();

			// Generate the new method implementation that calls into the runime.
			input.block = parse_quote!( { self.call_api_at(at, #name, &( #( #arg_names ),* )) } );
		}

		fold::fold_impl_item_method(self, input)
	}

	fn fold_item_impl(&mut self, mut input: ItemImpl) -> ItemImpl {
		// Implement the trait for the `RuntimeApi`
		input.self_ty = Box::new(parse_quote!( RuntimeApi ));

		// The implementation for the `RuntimeApi` is only required when compiling with the feature
		// `std` or `test`.
		input.attrs.push(parse_quote!( #[cfg(any(feature = "std", test))] ));

		fold::fold_item_impl(self, input)
	}
}

fn generate_api_impl_for_runtime_api(impls: &[ItemImpl]) -> Result<TokenStream> {
	let mut result = Vec::with_capacity(impls.len());

	for impl_ in impls {
		let runtime_block = extract_runtime_block_ident(extract_impl_trait(&impl_)?)?;
		let (node_block, node_block_id) = generate_node_block_and_block_id_ty(&impl_.self_ty);

		let mut visitor = ApiRuntimeImplToApiRuntimeApiImpl {
			runtime_block,
			node_block: &node_block,
			node_block_id: &node_block_id,
		};

		result.push(visitor.fold_item_impl(impl_.clone()));
	}

	Ok(quote!( #( #result )* ))
}

/// The implementation of the `impl_runtime_apis!` macro.
pub fn impl_runtime_apis_impl(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
	// Parse all impl blocks
	let RuntimeApiImpls { impls: api_impls } = parse_macro_input!(input as RuntimeApiImpls);
	let dispatch_impl = unwrap_or_error(generate_dispatch_function(&api_impls));
	let wasm_interface = unwrap_or_error(generate_wasm_interface(&api_impls));
	let hidden_includes = generate_hidden_includes(HIDDEN_INCLUDES_ID);
	let base_runtime_api = unwrap_or_error(generate_runtime_api_base_structures(&api_impls));
	let api_impls_for_runtime = unwrap_or_error(generate_api_impl_for_runtime(&api_impls));
	let api_impls_for_runtime_api = unwrap_or_error(generate_api_impl_for_runtime_api(&api_impls));

	quote!(
		#hidden_includes

		#base_runtime_api

		#api_impls_for_runtime

		#api_impls_for_runtime_api

		pub mod api {
			use super::*;

			#dispatch_impl

			#wasm_interface
		}
	).into()
}
