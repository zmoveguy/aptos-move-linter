// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

use crate::{
    abort_unless_arithmetics_enabled_for_structure, abort_unless_feature_flag_enabled,
    ark_binary_op_internal,
    natives::cryptography::algebra::{
        abort_invariant_violated, feature_flag_from_structure, AlgebraContext, Structure,
        E_TOO_MUCH_MEMORY_USED, MEMORY_LIMIT_IN_BYTES, MOVE_ABORT_CODE_NOT_IMPLEMENTED,
    },
    safe_borrow_element, store_element, structure_from_ty_arg,
};
use aptos_gas_schedule::gas_params::natives::aptos_framework::*;
use aptos_native_interface::{SafeNativeContext, SafeNativeError, SafeNativeResult};
use move_vm_types::{loaded_data::runtime_types::Type, values::Value};
use smallvec::{smallvec, SmallVec};
use std::{collections::VecDeque, ops::Mul, rc::Rc};

pub fn mul_internal(
    context: &mut SafeNativeContext,
    ty_args: Vec<Type>,
    mut args: VecDeque<Value>,
) -> SafeNativeResult<SmallVec<[Value; 1]>> {
    assert_eq!(1, ty_args.len());
    let structure_opt = structure_from_ty_arg!(context, &ty_args[0]);
    abort_unless_arithmetics_enabled_for_structure!(context, structure_opt);
    match structure_opt {
        Some(Structure::BLS12381Fr) => ark_binary_op_internal!(
            context,
            args,
            ark_bls12_381::Fr,
            mul,
            ALGEBRA_ARK_BLS12_381_FR_MUL
        ),
        Some(Structure::BLS12381Fq12) => ark_binary_op_internal!(
            context,
            args,
            ark_bls12_381::Fq12,
            mul,
            ALGEBRA_ARK_BLS12_381_FQ12_MUL
        ),
        Some(Structure::BN254Fr) => {
            ark_binary_op_internal!(context, args, ark_bn254::Fr, mul, ALGEBRA_ARK_BN254_FR_MUL)
        },
        Some(Structure::BN254Fq) => {
            ark_binary_op_internal!(context, args, ark_bn254::Fq, mul, ALGEBRA_ARK_BN254_FQ_MUL)
        },
        Some(Structure::BN254Fq12) => {
            ark_binary_op_internal!(
                context,
                args,
                ark_bn254::Fq12,
                mul,
                ALGEBRA_ARK_BN254_FQ12_MUL
            )
        },
        _ => Err(SafeNativeError::Abort {
            abort_code: MOVE_ABORT_CODE_NOT_IMPLEMENTED,
        }),
    }
}