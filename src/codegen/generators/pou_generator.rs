// Copyright (c) 2020 Ghaith Hachem and Mathias Rieder
use super::{
    data_type_generator::get_default_for,
    expression_generator::ExpressionCodeGenerator,
    llvm::{GlobalValueExt, Llvm},
    statement_generator::{FunctionContext, StatementCodeGenerator},
};
use crate::{
    ast::Pou,
    codegen::llvm_index::LlvmTypedIndex,
    diagnostics::{Diagnostic, INTERNAL_LLVM_ERROR},
    index::{self, ImplementationType},
    resolver::AstAnnotations,
};

/// The pou_generator contains functions to generate the code for POUs (PROGRAM, FUNCTION, FUNCTION_BLOCK)
/// # responsibilities
/// - generates a struct-datatype for the POU's members
/// - generates a function for the pou
/// - declares a global instance if the POU is a PROGRAM
use crate::index::{ImplementationIndexEntry, VariableIndexEntry};

use crate::{
    ast::{Implementation, PouType, SourceRange},
    index::Index,
};
use inkwell::types::{BasicType, StructType};
use inkwell::{
    module::Module,
    types::{BasicMetadataTypeEnum, BasicTypeEnum, FunctionType},
    values::{BasicValueEnum, FunctionValue},
    AddressSpace,
};

pub struct PouGenerator<'ink, 'cg> {
    llvm: Llvm<'ink>,
    index: &'cg Index,
    annotations: &'cg AstAnnotations,
    llvm_index: &'cg LlvmTypedIndex<'ink>,
}

/// Creates opaque implementations for all callable items in the index
/// Returns a Typed index containing the associated implementations.
pub fn generate_implementation_stubs<'ink>(
    module: &Module<'ink>,
    llvm: Llvm<'ink>,
    index: &Index,
    annotations: &AstAnnotations,
    types_index: &LlvmTypedIndex<'ink>,
) -> Result<LlvmTypedIndex<'ink>, Diagnostic> {
    let mut llvm_index = LlvmTypedIndex::default();
    let pou_generator = PouGenerator::new(llvm, index, annotations, types_index);
    for (name, implementation) in index.get_implementations() {
        if let Some(pou) = index.find_pou(implementation.get_call_name()) {
            if !pou.is_generic() {
                let curr_f = pou_generator.generate_implementation_stub(implementation, module)?;
                llvm_index.associate_implementation(name, curr_f)?;
            }
        }
    }

    Ok(llvm_index)
}

///Generates a global constant for each initialized pou member
/// The given constant can then be used to initialize the variable using memcpy without re-evaluating the expression
/// Retrieves the POUs from the index (implementation)
/// Returns a new LLVM index to be merged with the parent codegen index.
pub fn generate_global_constants_for_pou_members<'ink>(
    module: &Module<'ink>,
    llvm: &Llvm<'ink>,
    index: &Index,
    annotations: &AstAnnotations,
    llvm_index: &LlvmTypedIndex<'ink>,
) -> Result<LlvmTypedIndex<'ink>, Diagnostic> {
    let mut local_llvm_index = LlvmTypedIndex::default();
    for (_, implementation) in index.get_implementations() {
        let type_name = implementation.get_type_name();
        let pou_members = index.get_container_members(type_name);
        let variables = pou_members
            .iter()
            .filter(|it| it.is_local() || it.is_temp())
            .filter(|it| {
                let var_type = index
                    .get_effective_type_by_name(it.get_type_name())
                    .get_type_information();
                var_type.is_struct() || var_type.is_array() || var_type.is_string()
            });
        let exp_gen =
            ExpressionCodeGenerator::new_context_free(llvm, index, annotations, llvm_index);
        for variable in variables {
            let name = index::get_initializer_name(variable.get_qualified_name());
            let right_stmt = match variable.initial_value {
                Some(..) => Some(
                    index
                        .get_const_expressions()
                        .maybe_get_constant_statement(&variable.initial_value)
                        .ok_or_else(|| {
                            Diagnostic::cannot_generate_initializer(
                                variable.get_qualified_name(),
                                variable.source_location.clone(),
                            )
                        })?,
                ),
                None => None,
            };

            if let Some(stmt) = right_stmt {
                if llvm_index.find_global_value(&name).is_none() {
                    //Get either a global initial value for the constant (For arrays) and copy it,
                    let value = if let Some(value) =
                        llvm_index.find_associated_initial_value(variable.get_qualified_name())
                    {
                        value
                    } else {
                        exp_gen.generate_expression(stmt)?
                    };
                    let variable_type = llvm_index.get_associated_type(variable.get_type_name())?;
                    let global_value = llvm
                        .create_global_variable(module, &name, variable_type)
                        .make_constant()
                        .set_initial_value(Some(value), variable_type);
                    local_llvm_index.associate_global(&name, global_value)?;
                }
            }
        }
    }
    Ok(local_llvm_index)
}

impl<'ink, 'cg> PouGenerator<'ink, 'cg> {
    /// creates a new PouGenerator
    ///
    /// the PouGenerator needs a mutable index to register the generated pou
    pub fn new(
        llvm: Llvm<'ink>,
        index: &'cg Index,
        annotations: &'cg AstAnnotations,
        llvm_index: &'cg LlvmTypedIndex<'ink>,
    ) -> PouGenerator<'ink, 'cg> {
        PouGenerator {
            llvm,
            index,
            annotations,
            llvm_index,
        }
    }

    /// generates an empty llvm function for the given implementation, including all parameters and the return type
    pub fn generate_implementation_stub(
        &self,
        implementation: &ImplementationIndexEntry,
        module: &Module<'ink>,
    ) -> Result<FunctionValue<'ink>, Diagnostic> {
        let global_index = self.index;
        //generate a function that takes a instance-struct parameter
        let pou_name = implementation.get_call_name();

        let parameters = self.create_parameters_for_implementation(implementation)?;

        let return_type = match global_index.find_return_type(implementation.get_type_name()) {
            Some(r_type) => Some(self.llvm_index.get_associated_type(r_type.get_name())?),
            None => None,
        };

        let variadic = global_index
            .find_effective_type_info(implementation.get_type_name())
            .map(|it| it.is_variadic())
            .unwrap_or(false);

        let function_declaration =
            self.create_llvm_function_type(parameters, variadic, return_type)?;

        let curr_f = module.add_function(pou_name, function_declaration, None);
        Ok(curr_f)
    }

    /// creates and returns all parameters for the given implementation
    /// for functions, this method creates a full list of parameters, for other POUs
    /// this method creates a single state-struct parameter
    fn create_parameters_for_implementation(
        &self,
        implementation: &ImplementationIndexEntry,
    ) -> Result<Vec<BasicMetadataTypeEnum<'ink>>, Diagnostic> {
        if implementation.implementation_type != ImplementationType::Function {
            let mut parameters = vec![];
            if implementation.get_implementation_type() == &ImplementationType::Method {
                let class_name = implementation
                    .get_associated_class_name()
                    .expect("Method needs to have a class-name");
                let instance_members_struct_type: StructType = self
                    .llvm_index
                    .get_associated_type(class_name)
                    .map(|it| it.into_struct_type())?;
                parameters.push(
                    instance_members_struct_type
                        .ptr_type(AddressSpace::Generic)
                        .into(),
                );
            }
            let instance_struct_type: StructType = self
                .llvm_index
                .get_associated_pou_type(implementation.get_type_name())
                .map(|it| it.into_struct_type())?;
            parameters.push(instance_struct_type.ptr_type(AddressSpace::Generic).into());
            Ok(parameters)
        } else {
            //find the function's parameters
            self.index
                .get_container_members(implementation.get_call_name())
                .iter()
                .filter(|v| v.is_parameter())
                .map(|v| {
                    self.llvm_index
                        .get_associated_type(v.get_type_name())
                        .map(Into::into)
                })
                .collect::<Result<Vec<BasicMetadataTypeEnum>, _>>()
        }
    }

    /// generates a function for the given pou
    pub fn generate_implementation(
        &self,
        implementation: &Implementation,
    ) -> Result<(), Diagnostic> {
        let context = self.llvm.context;
        let mut local_index = LlvmTypedIndex::create_child(self.llvm_index);

        let pou_name = &implementation.name;

        let current_function = self
            .llvm_index
            .find_associated_implementation(pou_name)
            .ok_or_else(|| {
                Diagnostic::codegen_error(
                    &format!("Could not find generated stub for {}", pou_name),
                    implementation.location.clone(),
                )
            })?;

        //generate the body
        let block = context.append_basic_block(current_function, "entry");
        self.llvm.builder.position_at_end(block);

        let mut param_index = 0;

        if let PouType::Method { .. } = implementation.pou_type {
            let class_name = implementation.type_name.split('.').collect::<Vec<&str>>()[0];
            let class_members = self.index.get_container_members(class_name);
            self.generate_local_struct_variable_accessors(
                param_index,
                &mut local_index,
                class_name,
                current_function,
                &class_members,
            )?;
            param_index += 1;
        }

        let pou_members = self.index.get_container_members(&implementation.type_name);

        // generate local variables
        if implementation.pou_type == PouType::Function {
            self.generate_local_function_arguments_accessors(
                &mut local_index,
                &implementation.type_name,
                current_function,
                &pou_members,
            )?;
        } else {
            self.generate_local_struct_variable_accessors(
                param_index,
                &mut local_index,
                &implementation.type_name,
                current_function,
                &pou_members,
            )?;
        }

        let function_context = FunctionContext {
            linking_context: implementation.into(),
            function: current_function,
        };
        {
            //if this is a function, we need to initilialize the VAR-variables
            if matches!(
                implementation.pou_type,
                PouType::Function | PouType::Method { .. }
            ) {
                self.generate_initialization_of_local_vars(&pou_members, &local_index)?;
            } else {
                //Generate temp variables
                let members = pou_members
                    .into_iter()
                    .filter(|it| it.is_temp())
                    .collect::<Vec<&VariableIndexEntry>>();
                self.generate_initialization_of_local_vars(&members, &local_index)?;
            }
            let statement_gen = StatementCodeGenerator::new(
                &self.llvm,
                self.index,
                self.annotations,
                self,
                &local_index,
                &function_context,
            );
            statement_gen.generate_body(&implementation.statements)?
        }

        // generate return statement
        self.generate_return_statement(&function_context, &local_index)?;

        Ok(())
    }

    /// TODO llvm.rs
    /// generates a llvm `FunctionType` that takes the given list of `parameters` and
    /// returns the given `return_type`
    fn create_llvm_function_type(
        &self,
        parameters: Vec<BasicMetadataTypeEnum<'ink>>,
        is_var_args: bool,
        return_type: Option<BasicTypeEnum<'ink>>,
    ) -> Result<FunctionType<'ink>, Diagnostic> {
        let params = parameters.as_slice();
        match return_type {
            Some(enum_type) if enum_type.is_int_type() => {
                Ok(enum_type.into_int_type().fn_type(params, is_var_args))
            }
            Some(enum_type) if enum_type.is_float_type() => {
                Ok(enum_type.into_float_type().fn_type(params, is_var_args))
            }
            Some(enum_type) if enum_type.is_array_type() => {
                Ok(enum_type.into_array_type().fn_type(params, is_var_args))
            }
            Some(enum_type) if enum_type.is_pointer_type() => {
                Ok(enum_type.into_pointer_type().fn_type(params, is_var_args))
            }
            Some(enum_type) if enum_type.is_struct_type() => {
                Ok(enum_type.into_struct_type().fn_type(params, is_var_args))
            }
            None => Ok(self.llvm.context.void_type().fn_type(params, is_var_args)),
            _ => Err(Diagnostic::codegen_error(
                &format!("Unsupported return type {:?}", return_type),
                SourceRange::undefined(),
            )),
        }
    }

    /// generates a load-statement for the given members of a function
    fn generate_local_function_arguments_accessors(
        &self,
        index: &mut LlvmTypedIndex<'ink>,
        type_name: &str,
        current_function: FunctionValue<'ink>,
        members: &[&VariableIndexEntry],
    ) -> Result<(), Diagnostic> {
        //Generate reference to parameter
        // cannot use index from members because return and temp variables may not be considered for index in build_struct_gep
        let mut var_count = 0;
        for m in members.iter() {
            let parameter_name = m.get_name();

            let (name, variable) = if m.is_return() {
                let return_type = index.get_associated_type(m.get_type_name())?;
                (
                    Pou::calc_return_name(type_name),
                    self.llvm.create_local_variable(type_name, &return_type),
                )
            } else if m.is_parameter() {
                let ptr_value = current_function
                    .get_nth_param(var_count)
                    .ok_or_else(|| Diagnostic::missing_function(m.source_location.clone()))?;

                let ptr = self.llvm.create_local_variable(
                    m.get_name(),
                    &index.get_associated_type(m.get_type_name())?,
                );
                self.llvm.builder.build_store(ptr, ptr_value);

                var_count += 1;

                (parameter_name, ptr)
            } else {
                let temp_type = index.get_associated_type(m.get_type_name())?;
                (
                    parameter_name,
                    self.llvm.create_local_variable(parameter_name, &temp_type),
                )
            };

            index.associate_loaded_local_variable(type_name, name, variable)?;
        }

        Ok(())
    }

    /// generates a load-statement for the given members
    /// for pous that take a struct-state-variable (or two for methods)
    fn generate_local_struct_variable_accessors(
        &self,
        arg_index: u32,
        index: &mut LlvmTypedIndex<'ink>,
        type_name: &str,
        current_function: FunctionValue<'ink>,
        members: &[&VariableIndexEntry],
    ) -> Result<(), Diagnostic> {
        //Generate reference to parameter
        // cannot use index from members because return and temp variables may not be considered for index in build_struct_gep
        let mut var_count = 0;
        for m in members.iter() {
            let parameter_name = m.get_name();

            let (name, variable) = if m.is_temp() || m.is_return() {
                let temp_type = index.get_associated_type(m.get_type_name())?;
                (
                    parameter_name,
                    self.llvm.create_local_variable(parameter_name, &temp_type),
                )
            } else {
                let ptr_value = current_function
                    .get_nth_param(arg_index)
                    .map(BasicValueEnum::into_pointer_value)
                    .ok_or_else(|| Diagnostic::missing_function(m.source_location.clone()))?;

                let ptr = self
                    .llvm
                    .builder
                    .build_struct_gep(ptr_value, var_count as u32, parameter_name)
                    .expect(INTERNAL_LLVM_ERROR);

                var_count += 1;

                (parameter_name, ptr)
            };

            index.associate_loaded_local_variable(type_name, name, variable)?;
        }

        Ok(())
    }

    /// generates assignment statements for initialized variables in the VAR-block
    ///
    /// - `blocks` - all declaration blocks of the current pou
    fn generate_initialization_of_local_vars(
        &self,
        variables: &[&VariableIndexEntry],
        local_llvm_index: &LlvmTypedIndex,
    ) -> Result<(), Diagnostic> {
        let variables_with_initializers = variables
            .iter()
            .filter(|it| it.is_local() || it.is_temp() || it.is_return());

        let exp_gen = ExpressionCodeGenerator::new_context_free(
            &self.llvm,
            self.index,
            self.annotations,
            local_llvm_index,
        );

        for variable in variables_with_initializers {
            //get the loaded_ptr for the parameter and store right in it
            if let Some(left) = local_llvm_index
                .find_loaded_associated_variable_value(variable.get_qualified_name())
            {
                let right_stmt = match variable.initial_value {
                    Some(..) => Some(
                        self.index
                            .get_const_expressions()
                            .maybe_get_constant_statement(&variable.initial_value)
                            .ok_or_else(|| {
                                Diagnostic::cannot_generate_initializer(
                                    variable.get_qualified_name(),
                                    variable.source_location.clone(),
                                )
                            })?,
                    ),
                    None => None,
                };
                // for initializations we might have a global variable with the initial values
                // the idea is to memcpy the global variable
                let size = self
                    .llvm_index
                    .find_associated_type(variable.get_type_name())
                    .and_then(|associated_type| associated_type.size_of())
                    .ok_or("Couldn't determine type size");
                //First try to get a saved global constant
                let name = index::get_initializer_name(variable.get_qualified_name());
                let type_init_name = index::get_initializer_name(variable.get_type_name());
                if let Some(global_value) = self
                    .llvm_index
                    .find_global_value(&name)
                    .or_else(|| self.llvm_index.find_global_value(&type_init_name))
                {
                    size.and_then(|size| {
                        let alignment = std::cmp::max(1, global_value.get_alignment()); //TODO: This seems to always be 0
                        self.llvm.builder.build_memcpy(
                            left,
                            alignment,
                            global_value.as_pointer_value(),
                            alignment,
                            size,
                        )
                    })
                    .map_err(|err| {
                        Diagnostic::codegen_error(err, variable.source_location.clone())
                    })?;
                } else if left.get_type().get_element_type().is_array_type() {
                    //If nothint was found see if this is an array to set its value to 0
                    size.and_then(|size| {
                        self.llvm.builder.build_memset(
                            left,
                            1,
                            self.llvm.context.i8_type().const_zero(),
                            size,
                        )
                    })
                    .map_err(|it| {
                        Diagnostic::codegen_error(it, variable.source_location.clone())
                    })?;
                } else {
                    //Otherwise just generate a store expression
                    let value = if let Some(stmt) = right_stmt {
                        exp_gen.generate_expression(stmt)
                    } else {
                        self.llvm_index
                            .find_associated_type(variable.get_type_name())
                            .map(get_default_for)
                            .ok_or_else(|| {
                                Diagnostic::cannot_generate_initializer(
                                    variable.get_qualified_name(),
                                    variable.source_location.clone(),
                                )
                            })
                    }?;
                    self.llvm.builder.build_store(left, value);
                }
            } else {
                return Err(Diagnostic::cannot_generate_initializer(
                    variable.get_qualified_name(),
                    variable.source_location.clone(),
                ));
            }
        }
        Ok(())
    }

    /// generates the function's return statement only if the given pou_type is a `PouType::Function`
    ///
    /// a function returns the value of the local variable that has the function's name
    pub fn generate_return_statement(
        &self,
        function_context: &FunctionContext<'ink>,
        local_index: &LlvmTypedIndex<'ink>,
    ) -> Result<(), Diagnostic> {
        if let Some(ret_v) = self
            .index
            .find_return_variable(function_context.linking_context.get_type_name())
        {
            let call_name = function_context.linking_context.get_call_name();
            let var_name = format!("{}_ret", call_name);
            let ret_name = ret_v.get_qualified_name();
            let value_ptr = local_index
                .find_loaded_associated_variable_value(ret_name)
                .ok_or_else(|| {
                    Diagnostic::codegen_error(
                        &format!("Cannot generate return variable for {:}", call_name),
                        SourceRange::undefined(),
                    )
                })?;
            let loaded_value = self.llvm.load_pointer(&value_ptr, var_name.as_str());
            self.llvm.builder.build_return(Some(&loaded_value));
        } else {
            self.llvm.builder.build_return(None);
        }
        Ok(())
    }
}
