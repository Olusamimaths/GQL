use std::collections::HashMap;
use std::num::IntErrorKind;
use std::num::ParseIntError;
use std::vec;

use gitql_ast::expression::ArithmeticExpression;
use gitql_ast::expression::ArrayExpression;
use gitql_ast::expression::AssignmentExpression;
use gitql_ast::expression::BetweenExpression;
use gitql_ast::expression::BitwiseExpression;
use gitql_ast::expression::*;
use gitql_ast::operator::ArithmeticOperator;
use gitql_ast::operator::BinaryBitwiseOperator;
use gitql_ast::operator::BinaryLogicalOperator;
use gitql_ast::operator::ComparisonOperator;
use gitql_ast::operator::PrefixUnaryOperator;
use gitql_ast::statement::*;
use gitql_core::environment::Environment;
use gitql_core::types::DataType;
use gitql_core::value::Value;

use crate::context::ParserContext;
use crate::diagnostic::Diagnostic;
use crate::tokenizer::Location;
use crate::tokenizer::Token;
use crate::tokenizer::TokenKind;
use crate::type_checker::are_types_equals;
use crate::type_checker::check_all_values_are_same_type;
use crate::type_checker::check_function_call_arguments;
use crate::type_checker::is_expression_type_equals;
use crate::type_checker::prefix_unary_expected_type;
use crate::type_checker::resolve_call_expression_return_type;
use crate::type_checker::type_check_and_classify_selected_fields;
use crate::type_checker::type_check_prefix_unary;
use crate::type_checker::type_check_projection_symbols;
use crate::type_checker::type_mismatch_error;
use crate::type_checker::ExprTypeCheckResult;
use crate::type_checker::TypeCheckResult;

pub fn parse_gql(tokens: Vec<Token>, env: &mut Environment) -> Result<Query, Box<Diagnostic>> {
    let mut position = 0;
    let first_token = &tokens[position];
    let query_result = match &first_token.kind {
        TokenKind::Do => parse_do_query(env, &tokens, &mut position),
        TokenKind::Set => parse_set_query(env, &tokens, &mut position),
        TokenKind::Select => parse_select_query(env, &tokens, &mut position),
        TokenKind::Describe => parse_describe_query(env, &tokens, &mut position),
        TokenKind::Show => parse_show_query(&tokens, &mut position),
        _ => Err(un_expected_statement_error(&tokens, &mut position)),
    };

    // Consume optional `;` at the end of valid statement
    if let Some(last_token) = tokens.get(position) {
        if last_token.kind == TokenKind::Semicolon {
            position += 1;
        }
    }

    // Check for unexpected content after valid statement
    if query_result.is_ok() && position < tokens.len() {
        return Err(un_expected_content_after_correct_statement(
            &first_token.literal,
            &tokens,
            &mut position,
        ));
    }

    query_result
}

fn parse_do_query(
    env: &mut Environment,
    tokens: &[Token],
    position: &mut usize,
) -> Result<Query, Box<Diagnostic>> {
    // Consume Do keyword
    *position += 1;

    if *position >= tokens.len() {
        return Err(
            Diagnostic::error("Expect expression after Do Statement keyword")
                .with_location(get_safe_location(tokens, *position - 1))
                .as_boxed(),
        );
    }

    let mut context = ParserContext::default();
    let expression = parse_expression(&mut context, env, tokens, position)?;
    Ok(Query::Do(DoStatement { expression }))
}

fn parse_set_query(
    env: &mut Environment,
    tokens: &[Token],
    position: &mut usize,
) -> Result<Query, Box<Diagnostic>> {
    let len = tokens.len();
    let mut context = ParserContext::default();

    // Consume Set keyword
    *position += 1;

    if *position >= len || tokens[*position].kind != TokenKind::GlobalVariable {
        return Err(Diagnostic::error(
            "Expect Global variable name start with `@` after `SET` keyword",
        )
        .with_location(get_safe_location(tokens, *position - 1))
        .as_boxed());
    }

    let name = &tokens[*position].literal;

    // Consume variable name
    *position += 1;

    if *position >= len || !is_assignment_operator(&tokens[*position]) {
        return Err(
            Diagnostic::error("Expect `=` or `:=` and Value after Variable name")
                .with_location(get_safe_location(tokens, *position - 1))
                .as_boxed(),
        );
    }

    // Consume `=` or `:=` token
    *position += 1;

    let aggregations_count_before = context.aggregations.len();
    let value = parse_expression(&mut context, env, tokens, position)?;
    let has_aggregations = context.aggregations.len() != aggregations_count_before;

    // Until supports sub queries, aggregation value can't be stored in variables
    if has_aggregations {
        return Err(
            Diagnostic::error("Aggregation value can't be assigned to global variable")
                .with_location(get_safe_location(tokens, *position - 1))
                .as_boxed(),
        );
    }

    env.define_global(name.to_string(), value.expr_type(env));

    Ok(Query::GlobalVariableDeclaration(GlobalVariableStatement {
        name: name.to_string(),
        value,
    }))
}

fn parse_describe_query(
    env: &mut Environment,
    tokens: &[Token],
    position: &mut usize,
) -> Result<Query, Box<Diagnostic>> {
    // Consume `DESCRIBE` keyword
    *position += 1;

    if *position >= tokens.len() || tokens[*position].kind != TokenKind::Symbol {
        return Err(
            Diagnostic::error("Expect table name after DESCRIBE Statement")
                .with_location(get_safe_location(tokens, *position))
                .as_boxed(),
        );
    }

    // Make sure table name is valid
    let table_name = tokens[*position].literal.to_string();
    if !env
        .schema
        .tables_fields_names
        .contains_key(table_name.as_str())
    {
        return Err(
            Diagnostic::error(&format!("Unresolved table name `{}`", table_name))
                .add_help("You can use the command `SHOW TABLES` to get list of current tables")
                .with_location(get_safe_location(tokens, *position))
                .as_boxed(),
        );
    }

    // Consume Table Name
    *position += 1;

    Ok(Query::Describe(DescribeStatement { table_name }))
}

fn parse_show_query(tokens: &[Token], position: &mut usize) -> Result<Query, Box<Diagnostic>> {
    // Consume SHOW keyword
    *position += 1;

    if *position >= tokens.len() || tokens[*position].literal != "tables" {
        return Err(
            Diagnostic::error("Show can not be followed by names other than tables")
                .add_help("A correct statement will be `SHOW TABLES`")
                .with_location(get_safe_location(tokens, *position - 1))
                .as_boxed(),
        );
    }

    *position += 1;
    Ok(Query::ShowTables)
}

fn parse_select_query(
    env: &mut Environment,
    tokens: &[Token],
    position: &mut usize,
) -> Result<Query, Box<Diagnostic>> {
    let len = tokens.len();

    let mut context = ParserContext::default();
    let mut statements: HashMap<&'static str, Box<dyn Statement>> = HashMap::new();

    while *position < len {
        let token = &tokens[*position];

        match &token.kind {
            TokenKind::Select => {
                if statements.contains_key("select") {
                    return Err(Diagnostic::error("You already used `SELECT` statement")
                        .add_note("Can't use more than one `SELECT` statement in the same query")
                        .with_location(token.location)
                        .as_boxed());
                }
                let statement = parse_select_statement(&mut context, env, tokens, position)?;
                statements.insert("select", statement);
                context.is_single_value_query = !context.aggregations.is_empty();
                context.has_select_statement = true;
            }
            TokenKind::Where => {
                if statements.contains_key("where") {
                    return Err(Diagnostic::error("You already used `WHERE` statement")
                        .add_note("Can't use more than one `WHERE` statement in the same query")
                        .with_location(token.location)
                        .as_boxed());
                }

                let statement = parse_where_statement(&mut context, env, tokens, position)?;
                statements.insert("where", statement);
            }
            TokenKind::Group => {
                if statements.contains_key("group") {
                    return Err(Diagnostic::error("`You already used `GROUP BY` statement")
                        .add_note("Can't use more than one `GROUP BY` statement in the same query")
                        .with_location(token.location)
                        .as_boxed());
                }

                let statement = parse_group_by_statement(&mut context, env, tokens, position)?;
                statements.insert("group", statement);
            }
            TokenKind::Having => {
                if statements.contains_key("having") {
                    return Err(Diagnostic::error("You already used `HAVING` statement")
                        .add_note("Can't use more than one `HAVING` statement in the same query")
                        .with_location(token.location)
                        .as_boxed());
                }

                if !statements.contains_key("group") {
                    return Err(Diagnostic::error(
                        "`HAVING` must be used after `GROUP BY` statement",
                    )
                    .add_note(
                        "`HAVING` statement must be used in a query that has `GROUP BY` statement",
                    )
                    .with_location(token.location)
                    .as_boxed());
                }

                let statement = parse_having_statement(&mut context, env, tokens, position)?;
                statements.insert("having", statement);
            }
            TokenKind::Limit => {
                if statements.contains_key("limit") {
                    return Err(Diagnostic::error("You already used `LIMIT` statement")
                        .add_note("Can't use more than one `LIMIT` statement in the same query")
                        .with_location(token.location)
                        .as_boxed());
                }

                let statement = parse_limit_statement(tokens, position)?;
                statements.insert("limit", statement);

                // Check for Limit and Offset shortcut
                if *position < len && tokens[*position].kind == TokenKind::Comma {
                    // Prevent user from using offset statement more than one time
                    if statements.contains_key("offset") {
                        return Err(Diagnostic::error("You already used `OFFSET` statement")
                            .add_note(
                                "Can't use more than one `OFFSET` statement in the same query",
                            )
                            .with_location(token.location)
                            .as_boxed());
                    }

                    // Consume Comma
                    *position += 1;

                    if *position >= len || tokens[*position].kind != TokenKind::Integer {
                        return Err(Diagnostic::error(
                            "Expects `OFFSET` amount as Integer value after `,`",
                        )
                        .add_help("Try to add constant Integer after comma")
                        .add_note("`OFFSET` value must be a constant Integer")
                        .with_location(token.location)
                        .as_boxed());
                    }

                    let count_result: Result<usize, ParseIntError> =
                        tokens[*position].literal.parse();

                    // Report clear error for Integer parsing
                    if let Err(error) = &count_result {
                        if error.kind().eq(&IntErrorKind::PosOverflow) {
                            return Err(Diagnostic::error("`OFFSET` integer value is too large")
                                .add_help("Try to use smaller value")
                                .add_note(&format!(
                                    "`OFFSET` value must be between 0 and {}",
                                    usize::MAX
                                ))
                                .with_location(token.location)
                                .as_boxed());
                        }

                        return Err(Diagnostic::error("`OFFSET` integer value is invalid")
                            .add_help(&format!(
                                "`OFFSET` value must be between 0 and {}",
                                usize::MAX
                            ))
                            .with_location(token.location)
                            .as_boxed());
                    }

                    // Consume Offset value
                    *position += 1;

                    let count = count_result.unwrap();
                    statements.insert("offset", Box::new(OffsetStatement { count }));
                }
            }
            TokenKind::Offset => {
                if statements.contains_key("offset") {
                    return Err(Diagnostic::error("You already used `OFFSET` statement")
                        .add_note("Can't use more than one `OFFSET` statement in the same query")
                        .with_location(token.location)
                        .as_boxed());
                }

                let statement = parse_offset_statement(tokens, position)?;
                statements.insert("offset", statement);
            }
            TokenKind::Order => {
                if statements.contains_key("order") {
                    return Err(Diagnostic::error("You already used `ORDER BY` statement")
                        .add_note("Can't use more than one `ORDER BY` statement in the same query")
                        .with_location(token.location)
                        .as_boxed());
                }

                let statement = parse_order_by_statement(&mut context, env, tokens, position)?;
                statements.insert("order", statement);
            }
            TokenKind::Not => {
                return Err(Diagnostic::error(
                    "Expects `REGEXP` or `IN` expression after this `NOT` keyword",
                )
                .add_help("Try to use `REGEXP` or `IN` expression after NOT keyword")
                .add_help("Try to remove `NOT` keyword")
                .add_note("Expect to see `NOT` then `IN` keyword with a list of values")
                .with_location(get_safe_location(tokens, *position))
                .as_boxed())
            }
            _ => break,
        }
    }

    // If any aggregation function is used, add Aggregation Functions Node to the GQL Query
    if !context.aggregations.is_empty() {
        let aggregation_functions = AggregationsStatement {
            aggregations: context.aggregations,
        };
        statements.insert("aggregation", Box::new(aggregation_functions));
    }

    // Remove all selected fields from hidden selection
    let hidden_selections: Vec<String> = context
        .hidden_selections
        .iter()
        .filter(|n| !context.selected_fields.contains(n))
        .cloned()
        .collect();

    type_check_projection_symbols(
        env,
        &context.selected_tables,
        &context.projection_names,
        &context.projection_locations,
    )?;

    let hidden_selection_per_table =
        classify_hidden_selection(env, &context.selected_tables, &hidden_selections);

    Ok(Query::Select(GQLQuery {
        statements,
        has_aggregation_function: context.is_single_value_query,
        has_group_by_statement: context.has_group_by_statement,
        hidden_selections: hidden_selection_per_table,
        alias_table: context.name_alias_table,
    }))
}

/// Classify hidden selection per table
fn classify_hidden_selection(
    env: &mut Environment,
    tables: &[String],
    hidden_selections: &[String],
) -> HashMap<String, Vec<String>> {
    let mut table_hidden_selections: HashMap<String, Vec<String>> = HashMap::new();
    for table in tables {
        table_hidden_selections.insert(table.to_string(), vec![]);
    }

    for hidden_selection in hidden_selections {
        let mut is_resolved = false;
        for table in tables {
            let table_columns = env.schema.tables_fields_names.get(table.as_str()).unwrap();
            if table_columns.contains(&hidden_selection.as_str()) {
                let hidden_selection_for_table = table_hidden_selections.get_mut(table).unwrap();
                if !hidden_selection_for_table.contains(hidden_selection) {
                    hidden_selection_for_table.push(hidden_selection.to_string());
                    is_resolved = true;
                    break;
                }
            }
        }

        // If this symbol is not column name, maybe generated column
        if !is_resolved && !table_hidden_selections.is_empty() {
            table_hidden_selections
                .get_mut(&tables[0])
                .unwrap()
                .push(hidden_selection.to_string());
        }
    }

    table_hidden_selections
}

fn parse_select_statement(
    context: &mut ParserContext,
    env: &mut Environment,
    tokens: &[Token],
    position: &mut usize,
) -> Result<Box<dyn Statement>, Box<Diagnostic>> {
    // Consume select keyword
    *position += 1;

    if *position >= tokens.len() {
        return Err(Diagnostic::error("Incomplete input for select statement")
            .add_help("Try select one or more values in the `SELECT` statement")
            .add_note("Select statements requires at least selecting one value")
            .with_location(get_safe_location(tokens, *position - 1))
            .as_boxed());
    }

    let mut tables_to_select_from: Vec<String> = vec![];
    let mut fields_names: Vec<String> = vec![];
    let mut selected_expr_titles: Vec<String> = vec![];
    let mut selected_expr: Vec<Box<dyn Expression>> = vec![];

    let mut is_select_all = false;

    let distinct = parse_select_distinct_option(context, tokens, position)?;

    // Select all option
    if *position < tokens.len() && tokens[*position].kind == TokenKind::Star {
        // Consume `*`
        *position += 1;
        is_select_all = true;
    } else {
        while *position < tokens.len() && tokens[*position].kind != TokenKind::From {
            let expression = parse_expression(context, env, tokens, position)?;
            let expr_type = expression.expr_type(env).clone();
            let field_name = if let Some(expression_literal) = expression_literal(&expression) {
                expression_literal
            } else {
                context.generate_column_name()
            };

            // Assert that each selected field is unique
            if fields_names.contains(&field_name) {
                return Err(Diagnostic::error("Can't select the same field twice")
                    .with_location(get_safe_location(tokens, *position - 1))
                    .as_boxed());
            }

            // Check for Field name alias
            if *position < tokens.len() && tokens[*position].kind == TokenKind::As {
                // Consume `as` keyword
                *position += 1;
                let alias_name_token = consume_kind(tokens, *position, TokenKind::Symbol);
                if alias_name_token.is_err() {
                    return Err(Diagnostic::error("Expect `identifier` as field alias name")
                        .with_location(get_safe_location(tokens, *position))
                        .as_boxed());
                }

                // Register alias name
                let alias_name = alias_name_token.ok().unwrap().literal.to_string();
                if context.selected_fields.contains(&alias_name)
                    || context.name_alias_table.contains_key(&alias_name)
                {
                    return Err(
                        Diagnostic::error("You already have field with the same name")
                            .add_help("Try to use a new unique name for alias")
                            .with_location(get_safe_location(tokens, *position))
                            .as_boxed(),
                    );
                }

                // Consume alias name
                *position += 1;

                // Register alias name type
                env.define(alias_name.to_string(), expr_type.clone());

                context.selected_fields.push(alias_name.clone());
                context
                    .name_alias_table
                    .insert(field_name.to_string(), alias_name.to_string());
                selected_expr_titles.push(alias_name.to_owned());
            } else {
                selected_expr_titles.push(field_name.to_owned());
            }

            // Register field type
            env.define(field_name.to_string(), expr_type);

            fields_names.push(field_name.to_owned());
            context.selected_fields.push(field_name.to_owned());

            selected_expr.push(expression);

            // Consume `,` or break
            if *position < tokens.len() && tokens[*position].kind == TokenKind::Comma {
                *position += 1;
            } else {
                break;
            }
        }
    }

    // Parse optional Form statement
    let mut joins: Vec<Join> = vec![];
    if *position < tokens.len() && tokens[*position].kind == TokenKind::From {
        // Consume `from` keyword
        *position += 1;

        let table_name_token = consume_kind(tokens, *position, TokenKind::Symbol);
        if table_name_token.is_err() {
            return Err(Diagnostic::error("Expect `identifier` as a table name")
                .add_note("Table name must be an identifier")
                .with_location(get_safe_location(tokens, *position))
                .as_boxed());
        }

        let table_name = &table_name_token.ok().unwrap().literal;
        if !env
            .schema
            .tables_fields_names
            .contains_key(table_name.as_str())
        {
            return Err(Diagnostic::error("Unresolved table name")
                .add_help("Check the documentations to see available tables")
                .with_location(get_safe_location(tokens, *position))
                .as_boxed());
        }

        tables_to_select_from.push(table_name.to_string());
        context.selected_tables.push(table_name.to_string());

        // Consume table name
        *position += 1;

        // Parse Joins
        // Convert it to white when supporting Multi different joins
        if *position < tokens.len() && is_join_token(&tokens[*position]) {
            let join_token = &tokens[*position];

            // The default join type now is cross join because we don't support `ON` Condition
            let mut join_kind = JoinKind::Cross;
            if join_token.kind != TokenKind::Join {
                join_kind = match join_token.kind {
                    TokenKind::Left => JoinKind::Left,
                    TokenKind::Right => JoinKind::Right,
                    TokenKind::Cross => JoinKind::Cross,
                    _ => JoinKind::Inner,
                };

                // TODO: Remove it after support all types in the engine level and support `ON` Condition
                if join_kind != JoinKind::Cross {
                    return Err(Diagnostic::error(
                        "Unfortunately only `CROSS JOIN` is supported for now",
                    )
                    .with_location(get_safe_location(tokens, *position))
                    .as_boxed());
                }

                // Consume Left, Right, Inner or Cross
                *position += 1;

                if *position >= tokens.len() || tokens[*position].kind != TokenKind::Join {
                    return Err(Diagnostic::error(
                        "Expect `JOIN` keyword after Cross, Left, Right, Inner",
                    )
                    .with_location(get_safe_location(tokens, *position))
                    .as_boxed());
                }
            }

            // Consume Join keyword
            *position += 1;
            if *position >= tokens.len() || tokens[*position].kind != TokenKind::Symbol {
                return Err(Diagnostic::error("Expect table name after `JOIN` keyword")
                    .with_location(get_safe_location(tokens, *position))
                    .as_boxed());
            }

            let other_table = &tokens[*position];
            let other_table_name = &other_table.literal;

            // TODO: will be useful after support table alias `table as t` and dot expression
            if table_name == other_table_name {
                return Err(Diagnostic::error(
                    "The two tables of join must be unique or have different alias",
                )
                .with_location(get_safe_location(tokens, *position))
                .as_boxed());
            }

            tables_to_select_from.push(other_table_name.to_string());
            context.selected_tables.push(other_table_name.to_string());
            register_current_table_fields_types(env, other_table_name);

            // Consume Other table name
            *position += 1;

            // TODO: must be removed after support `ON`
            if *position < tokens.len() && tokens[*position].kind == TokenKind::On {
                return Err(
                    Diagnostic::error("The `ON` keyword after join is not supported yet")
                        .with_location(get_safe_location(tokens, *position))
                        .as_boxed(),
                );
            }

            joins.push(Join {
                right: table_name.to_string(),
                left: other_table_name.to_string(),
                kind: join_kind,
                predicate: None,
            })
        }

        register_current_table_fields_types(env, table_name);
    }

    // Make sure Aggregated functions are used with tables only
    if tables_to_select_from.is_empty() && !context.aggregations.is_empty() {
        return Err(
            Diagnostic::error("Aggregations functions should be used only with tables")
                .add_note("Try to select from one of the available tables in current schema")
                .with_location(get_safe_location(tokens, *position))
                .as_boxed(),
        );
    }

    // Make sure `SELECT *` used with specific table
    if is_select_all && tables_to_select_from.is_empty() {
        return Err(
            Diagnostic::error("Expect `FROM` and table name after `SELECT *`")
                .add_help("Select all must be used with valid table name")
                .with_location(get_safe_location(tokens, *position))
                .as_boxed(),
        );
    }

    // Select input validations
    if !is_select_all && fields_names.is_empty() {
        return Err(Diagnostic::error("Incomplete input for select statement")
            .add_help("Try select one or more values in the `SELECT` statement")
            .add_note("Select statements requires at least selecting one value")
            .with_location(get_safe_location(tokens, *position - 1))
            .as_boxed());
    }

    // If it `select *` make all table fields selectable
    if is_select_all {
        select_all_table_fields(
            env,
            &tables_to_select_from,
            &mut context.selected_fields,
            &mut fields_names,
        );
    }

    // Type check all selected fields has type registered in type table
    let table_selections = type_check_and_classify_selected_fields(
        env,
        &tables_to_select_from,
        &fields_names,
        get_safe_location(tokens, *position),
    )?;

    Ok(Box::new(SelectStatement {
        table_selections,
        joins,
        selected_expr_titles,
        selected_expr,
        distinct,
    }))
}

fn parse_select_distinct_option(
    context: &mut ParserContext,
    tokens: &[Token],
    position: &mut usize,
) -> Result<Distinct, Box<Diagnostic>> {
    if tokens[*position].kind == TokenKind::Distinct {
        // Consume `DISTINCT` keyword
        *position += 1;

        if *position < tokens.len() && tokens[*position].kind == TokenKind::On {
            // Consume `ON` keyword
            *position += 1;

            if *position >= tokens.len() || tokens[*position].kind != TokenKind::LeftParen {
                return Err(Diagnostic::error("Expect `(` after `DISTINCT ON`")
                    .add_help("Try to add `(` after ON and before fields")
                    .with_location(get_safe_location(tokens, *position))
                    .as_boxed());
            }

            // Consume `(` Left Parenthesis
            *position += 1;

            let mut distinct_fields: Vec<String> = vec![];
            while *position < tokens.len() && tokens[*position].kind != TokenKind::RightParen {
                let field_token = &tokens[*position];
                let literal = &field_token.literal;
                let location = field_token.location;

                distinct_fields.push(literal.to_string());

                context.hidden_selections.push(literal.to_string());
                context.projection_names.push(literal.to_string());
                context.projection_locations.push(location);

                // Consume field name
                *position += 1;

                if *position < tokens.len() && tokens[*position].kind == TokenKind::Comma {
                    *position += 1;
                } else {
                    break;
                }
            }

            if *position >= tokens.len() || tokens[*position].kind != TokenKind::RightParen {
                return Err(Diagnostic::error("Expect `)` after `DISTINCT ON fields`")
                    .add_help("Try to add `)` after fields")
                    .with_location(get_safe_location(tokens, *position))
                    .as_boxed());
            }

            // Consume `)` Right Parenthesis
            *position += 1;

            // Prevent passing empty fields
            if distinct_fields.is_empty() {
                return Err(Diagnostic::error(
                    "DISTINCT ON(...) must be used with one of more column",
                )
                .add_help("Try to add one or more columns from current table")
                .with_location(get_safe_location(tokens, *position))
                .as_boxed());
            }

            // Prevent user from writing comma after DISTINCT ON
            if *position < tokens.len() && tokens[*position].kind == TokenKind::Comma {
                return Err(
                    Diagnostic::error("No need to add Comma `,` after DISTINCT ON")
                        .add_help("Try to remove `,` after DISTINCT ON fields")
                        .with_location(get_safe_location(tokens, *position))
                        .as_boxed(),
                );
            }

            return Ok(Distinct::DistinctOn(distinct_fields));
        }
        return Ok(Distinct::DistinctAll);
    }

    Ok(Distinct::None)
}

fn parse_where_statement(
    context: &mut ParserContext,
    env: &mut Environment,
    tokens: &[Token],
    position: &mut usize,
) -> Result<Box<dyn Statement>, Box<Diagnostic>> {
    *position += 1;
    if *position >= tokens.len() {
        return Err(Diagnostic::error("Expect expression after `WHERE` keyword")
            .add_help("Try to add boolean expression after `WHERE` keyword")
            .add_note("`WHERE` statement expects expression as condition")
            .with_location(get_safe_location(tokens, *position - 1))
            .as_boxed());
    }

    let aggregations_count_before = context.aggregations.len();

    // Make sure WHERE condition expression has boolean type or can implicit casted to boolean
    let condition_location = tokens[*position].location;
    let mut condition = parse_expression(context, env, tokens, position)?;
    match is_expression_type_equals(env, &condition, &DataType::Boolean) {
        ExprTypeCheckResult::ImplicitCasted(expr) => {
            condition = expr;
        }
        ExprTypeCheckResult::Error(diagnostic) => {
            return Err(diagnostic);
        }
        ExprTypeCheckResult::NotEqualAndCantImplicitCast => {
            return Err(Diagnostic::error(&format!(
                "Expect `WHERE` condition to be type {} but got {}",
                DataType::Boolean,
                condition.expr_type(env)
            ))
            .add_note("`WHERE` statement condition must be Boolean")
            .with_location(condition_location)
            .as_boxed());
        }
        _ => {}
    }

    let aggregations_count_after = context.aggregations.len();
    if aggregations_count_before != aggregations_count_after {
        return Err(
            Diagnostic::error("Can't use Aggregation functions in `WHERE` statement")
                .add_note("Aggregation functions must be used after `GROUP BY` statement")
                .add_note("Aggregation functions evaluated after later after `GROUP BY` statement")
                .with_location(condition_location)
                .as_boxed(),
        );
    }

    Ok(Box::new(WhereStatement { condition }))
}

fn parse_group_by_statement(
    context: &mut ParserContext,
    env: &mut Environment,
    tokens: &[Token],
    position: &mut usize,
) -> Result<Box<dyn Statement>, Box<Diagnostic>> {
    // Consume `Group` keyword
    *position += 1;

    if *position >= tokens.len() || tokens[*position].kind != TokenKind::By {
        return Err(
            Diagnostic::error("Expect keyword `by` after keyword `group`")
                .add_help("Try to use `BY` keyword after `GROUP")
                .with_location(get_safe_location(tokens, *position - 1))
                .as_boxed(),
        );
    }

    // Consume `By` keyword
    *position += 1;

    // Parse one or more expression
    let mut values: Vec<Box<dyn Expression>> = vec![];
    while *position < tokens.len() {
        values.push(parse_expression(context, env, tokens, position)?);

        if *position < tokens.len() && tokens[*position].kind == TokenKind::Comma {
            // Consume Comma `,`
            *position += 1;
            continue;
        }

        break;
    }

    context.has_group_by_statement = true;
    Ok(Box::new(GroupByStatement { values }))
}

fn parse_having_statement(
    context: &mut ParserContext,
    env: &mut Environment,
    tokens: &[Token],
    position: &mut usize,
) -> Result<Box<dyn Statement>, Box<Diagnostic>> {
    *position += 1;
    if *position >= tokens.len() {
        return Err(
            Diagnostic::error("Expect expression after `HAVING` keyword")
                .add_help("Try to add boolean expression after `HAVING` keyword")
                .add_note("`HAVING` statement expects expression as condition")
                .with_location(get_safe_location(tokens, *position - 1))
                .as_boxed(),
        );
    }

    // Make sure HAVING condition expression has boolean type
    let condition_location = tokens[*position].location;
    let mut condition = parse_expression(context, env, tokens, position)?;
    match is_expression_type_equals(env, &condition, &DataType::Boolean) {
        ExprTypeCheckResult::ImplicitCasted(expr) => {
            condition = expr;
        }
        ExprTypeCheckResult::Error(diagnostic) => {
            return Err(diagnostic);
        }
        ExprTypeCheckResult::NotEqualAndCantImplicitCast => {
            return Err(Diagnostic::error(&format!(
                "Expect `HAVING` condition to be type {} but got {}",
                DataType::Boolean,
                condition.expr_type(env)
            ))
            .add_note("`HAVING` statement condition must be Boolean")
            .with_location(condition_location)
            .as_boxed());
        }
        _ => {}
    }

    Ok(Box::new(HavingStatement { condition }))
}

fn parse_limit_statement(
    tokens: &[Token],
    position: &mut usize,
) -> Result<Box<dyn Statement>, Box<Diagnostic>> {
    *position += 1;
    if *position >= tokens.len() || tokens[*position].kind != TokenKind::Integer {
        return Err(Diagnostic::error("Expect number after `LIMIT` keyword")
            .with_location(get_safe_location(tokens, *position - 1))
            .as_boxed());
    }

    let count_result: Result<usize, ParseIntError> = tokens[*position].literal.parse();

    // Report clear error for Integer parsing
    if let Err(error) = &count_result {
        if error.kind().eq(&IntErrorKind::PosOverflow) {
            return Err(Diagnostic::error("`LIMIT` integer value is too large")
                .add_help("Try to use smaller value")
                .add_note(&format!(
                    "`LIMIT` value must be between 0 and {}",
                    usize::MAX
                ))
                .with_location(get_safe_location(tokens, *position))
                .as_boxed());
        }

        return Err(Diagnostic::error("`LIMIT` integer value is invalid")
            .add_help(&format!(
                "`LIMIT` value must be between 0 and {}",
                usize::MAX
            ))
            .with_location(get_safe_location(tokens, *position))
            .as_boxed());
    }

    // Consume Integer value
    *position += 1;

    let count = count_result.unwrap();
    Ok(Box::new(LimitStatement { count }))
}

fn parse_offset_statement(
    tokens: &[Token],
    position: &mut usize,
) -> Result<Box<dyn Statement>, Box<Diagnostic>> {
    *position += 1;
    if *position >= tokens.len() || tokens[*position].kind != TokenKind::Integer {
        return Err(Diagnostic::error("Expect number after `OFFSET` keyword")
            .with_location(get_safe_location(tokens, *position - 1))
            .as_boxed());
    }

    let count_result: Result<usize, ParseIntError> = tokens[*position].literal.parse();

    // Report clear error for Integer parsing
    if let Err(error) = &count_result {
        if error.kind().eq(&IntErrorKind::PosOverflow) {
            return Err(Diagnostic::error("`OFFSET` integer value is too large")
                .add_help("Try to use smaller value")
                .add_note(&format!(
                    "`OFFSET` value must be between 0 and {}",
                    usize::MAX
                ))
                .with_location(get_safe_location(tokens, *position))
                .as_boxed());
        }

        return Err(Diagnostic::error("`OFFSET` integer value is invalid")
            .add_help(&format!(
                "`OFFSET` value must be between 0 and {}",
                usize::MAX
            ))
            .with_location(get_safe_location(tokens, *position))
            .as_boxed());
    }

    *position += 1;

    let count = count_result.unwrap();
    Ok(Box::new(OffsetStatement { count }))
}

fn parse_order_by_statement(
    context: &mut ParserContext,
    env: &mut Environment,
    tokens: &[Token],
    position: &mut usize,
) -> Result<Box<dyn Statement>, Box<Diagnostic>> {
    // Consume `ORDER` keyword
    *position += 1;

    if *position >= tokens.len() || tokens[*position].kind != TokenKind::By {
        return Err(
            Diagnostic::error("Expect keyword `BY` after keyword `ORDER")
                .add_help("Try to use `BY` keyword after `ORDER")
                .with_location(get_safe_location(tokens, *position - 1))
                .as_boxed(),
        );
    }

    // Consume `BY` keyword
    *position += 1;

    let mut arguments: Vec<Box<dyn Expression>> = vec![];
    let mut sorting_orders: Vec<SortingOrder> = vec![];

    loop {
        let argument = parse_expression(context, env, tokens, position)?;
        arguments.push(argument);

        let mut order = SortingOrder::Ascending;
        if *position < tokens.len() && is_asc_or_desc(&tokens[*position]) {
            if tokens[*position].kind == TokenKind::Descending {
                order = SortingOrder::Descending;
            }

            // Consume `ASC or DESC` keyword
            *position += 1;
        }

        sorting_orders.push(order);
        if *position < tokens.len() && tokens[*position].kind == TokenKind::Comma {
            // Consume `,` keyword
            *position += 1;
        } else {
            break;
        }
    }

    Ok(Box::new(OrderByStatement {
        arguments,
        sorting_orders,
    }))
}

fn parse_expression(
    context: &mut ParserContext,
    env: &mut Environment,
    tokens: &[Token],
    position: &mut usize,
) -> Result<Box<dyn Expression>, Box<Diagnostic>> {
    let aggregations_count_before = context.aggregations.len();
    let expression = parse_assignment_expression(context, env, tokens, position)?;
    let has_aggregations = context.aggregations.len() != aggregations_count_before;

    if has_aggregations {
        let column_name = context.generate_column_name();
        env.define(column_name.to_string(), expression.expr_type(env));

        // Register the new aggregation generated field if the this expression is after group by
        if context.has_group_by_statement && !context.hidden_selections.contains(&column_name) {
            context.hidden_selections.push(column_name.to_string());
        }

        context
            .aggregations
            .insert(column_name.clone(), AggregateValue::Expression(expression));

        return Ok(Box::new(SymbolExpression { value: column_name }));
    }

    Ok(expression)
}

fn parse_assignment_expression(
    context: &mut ParserContext,
    env: &mut Environment,
    tokens: &[Token],
    position: &mut usize,
) -> Result<Box<dyn Expression>, Box<Diagnostic>> {
    let expression = parse_regex_expression(context, env, tokens, position)?;
    if *position < tokens.len() && tokens[*position].kind == TokenKind::ColonEqual {
        if expression.kind() != ExpressionKind::GlobalVariable {
            return Err(Diagnostic::error(
                "Assignment expressions expect global variable name before `:=`",
            )
            .with_location(tokens[*position].location)
            .as_boxed());
        }

        let expr = expression
            .as_any()
            .downcast_ref::<GlobalVariableExpression>()
            .unwrap();

        let variable_name = expr.name.to_string();

        // Consume `:=` operator
        *position += 1;

        let value = parse_regex_expression(context, env, tokens, position)?;
        env.define_global(variable_name.clone(), value.expr_type(env));

        return Ok(Box::new(AssignmentExpression {
            symbol: variable_name.clone(),
            value,
        }));
    }
    Ok(expression)
}

fn parse_regex_expression(
    context: &mut ParserContext,
    env: &mut Environment,
    tokens: &[Token],
    position: &mut usize,
) -> Result<Box<dyn Expression>, Box<Diagnostic>> {
    let expression = parse_is_null_expression(context, env, tokens, position)?;

    // Consume NOT if current token is `RegExp` and next one is `IN`
    let has_not_keyword = if *position < tokens.len() - 1
        && tokens[*position].kind == TokenKind::Not
        && tokens[*position + 1].kind == TokenKind::RegExp
    {
        *position += 1;
        true
    } else {
        false
    };

    if *position < tokens.len() && tokens[*position].kind == TokenKind::RegExp {
        if !expression.expr_type(env).is_text() {
            return Err(
                Diagnostic::error("`REGEXP` left hand side must be `Text` Type")
                    .with_location(tokens[*position].location)
                    .as_boxed(),
            );
        }

        *position += 1;

        let pattern = parse_is_null_expression(context, env, tokens, position)?;
        if !pattern.expr_type(env).is_text() {
            return Err(
                Diagnostic::error("`REGEXP` right hand side must be `Text` Type")
                    .with_location(tokens[*position].location)
                    .as_boxed(),
            );
        }

        let regex_expr = Box::new(RegexExpression {
            input: expression,
            pattern,
        });

        return Ok(if has_not_keyword {
            Box::new(UnaryExpression {
                right: regex_expr,
                op: PrefixUnaryOperator::Bang,
            })
        } else {
            regex_expr
        });
    }

    Ok(expression)
}

fn parse_is_null_expression(
    context: &mut ParserContext,
    env: &mut Environment,
    tokens: &[Token],
    position: &mut usize,
) -> Result<Box<dyn Expression>, Box<Diagnostic>> {
    let expression = parse_in_expression(context, env, tokens, position)?;
    if *position < tokens.len() && tokens[*position].kind == TokenKind::Is {
        let is_location = tokens[*position].location;

        // Consume `IS` keyword
        *position += 1;

        let has_not_keyword =
            if *position < tokens.len() && tokens[*position].kind == TokenKind::Not {
                // Consume `NOT` keyword
                *position += 1;
                true
            } else {
                false
            };

        if *position < tokens.len() && tokens[*position].kind == TokenKind::Null {
            // Consume `Null` keyword
            *position += 1;

            return Ok(Box::new(IsNullExpression {
                argument: expression,
                has_not: has_not_keyword,
            }));
        }

        return Err(
            Diagnostic::error("Expects `NULL` Keyword after `IS` or `IS NOT`")
                .with_location(is_location)
                .as_boxed(),
        );
    }
    Ok(expression)
}

fn parse_in_expression(
    context: &mut ParserContext,
    env: &mut Environment,
    tokens: &[Token],
    position: &mut usize,
) -> Result<Box<dyn Expression>, Box<Diagnostic>> {
    let expression = parse_between_expression(context, env, tokens, position)?;

    // Consume NOT if current token is `NOT` and next one is `IN`
    let has_not_keyword = if *position < tokens.len() - 1
        && tokens[*position].kind == TokenKind::Not
        && tokens[*position + 1].kind == TokenKind::In
    {
        *position += 1;
        true
    } else {
        false
    };

    if *position < tokens.len() && tokens[*position].kind == TokenKind::In {
        let in_location = tokens[*position].location;

        // Consume `IN` keyword
        *position += 1;

        if consume_kind(tokens, *position, TokenKind::LeftParen).is_err() {
            return Err(
                Diagnostic::error("Expects values between `(` and `)` after `IN` keyword")
                    .with_location(in_location)
                    .as_boxed(),
            );
        }

        let values = parse_arguments_expressions(context, env, tokens, position)?;

        // Optimize the Expression if the number of values in the list is 0
        if values.is_empty() {
            return Ok(Box::new(BooleanExpression {
                is_true: has_not_keyword,
            }));
        }

        let values_type_result = check_all_values_are_same_type(env, &values);
        if values_type_result.is_none() {
            return Err(Diagnostic::error(
                "Expects values between `(` and `)` to have the same type",
            )
            .with_location(in_location)
            .as_boxed());
        }

        // Check that argument and values has the same type
        let values_type = values_type_result.unwrap();
        if values_type != DataType::Any && expression.expr_type(env) != values_type {
            return Err(Diagnostic::error(
                "Argument and Values of In Expression must have the same type",
            )
            .with_location(in_location)
            .as_boxed());
        }

        return Ok(Box::new(InExpression {
            argument: expression,
            values,
            values_type,
            has_not_keyword,
        }));
    }

    Ok(expression)
}

fn parse_between_expression(
    context: &mut ParserContext,
    env: &mut Environment,
    tokens: &[Token],
    position: &mut usize,
) -> Result<Box<dyn Expression>, Box<Diagnostic>> {
    let expression = parse_logical_or_expression(context, env, tokens, position)?;
    if *position < tokens.len() && tokens[*position].kind == TokenKind::Between {
        let between_location = tokens[*position].location;

        // Consume `BETWEEN` keyword
        *position += 1;

        if *position >= tokens.len() {
            return Err(
                Diagnostic::error("`BETWEEN` keyword expects two range after it")
                    .with_location(between_location)
                    .as_boxed(),
            );
        }

        let argument_type = expression.expr_type(env);
        let range_start = parse_logical_or_expression(context, env, tokens, position)?;

        if *position >= tokens.len() || tokens[*position].kind != TokenKind::DotDot {
            return Err(Diagnostic::error("Expect `..` after `BETWEEN` range start")
                .with_location(between_location)
                .as_boxed());
        }

        // Consume `..` token
        *position += 1;

        let range_end = parse_logical_or_expression(context, env, tokens, position)?;

        if argument_type != range_start.expr_type(env) || argument_type != range_end.expr_type(env)
        {
            return Err(Diagnostic::error(&format!(
                "Expect `BETWEEN` argument, range start and end to has same type but got {}, {} and {}",
                argument_type,
                range_start.expr_type(env),
                range_end.expr_type(env)
            ))
            .add_help("Try to make sure all of them has same type")
            .with_location(between_location)
            .as_boxed());
        }

        return Ok(Box::new(BetweenExpression {
            value: expression,
            range_start,
            range_end,
        }));
    }

    Ok(expression)
}

fn parse_logical_or_expression(
    context: &mut ParserContext,
    env: &mut Environment,
    tokens: &[Token],
    position: &mut usize,
) -> Result<Box<dyn Expression>, Box<Diagnostic>> {
    let mut lhs = parse_logical_and_expression(context, env, tokens, position)?;
    while *position < tokens.len() && tokens[*position].kind == TokenKind::LogicalOr {
        let expected_type = DataType::Boolean;
        match is_expression_type_equals(env, &lhs, &expected_type) {
            ExprTypeCheckResult::ImplicitCasted(expr) => lhs = expr,
            ExprTypeCheckResult::Error(diagnostic) => return Err(diagnostic),
            ExprTypeCheckResult::NotEqualAndCantImplicitCast => {
                return Err(type_mismatch_error(
                    tokens[*position - 1].location,
                    expected_type,
                    lhs.expr_type(env),
                )
                .as_boxed());
            }
            _ => {}
        }

        // Consume operator
        *position += 1;

        let mut rhs = parse_logical_and_expression(context, env, tokens, position)?;
        match is_expression_type_equals(env, &rhs, &expected_type) {
            ExprTypeCheckResult::ImplicitCasted(expr) => rhs = expr,
            ExprTypeCheckResult::Error(diagnostic) => return Err(diagnostic),
            ExprTypeCheckResult::NotEqualAndCantImplicitCast => {
                return Err(type_mismatch_error(
                    tokens[*position].location,
                    expected_type,
                    rhs.expr_type(env),
                )
                .as_boxed());
            }
            _ => {}
        }

        lhs = Box::new(LogicalExpression {
            left: lhs,
            operator: BinaryLogicalOperator::Or,
            right: rhs,
        });
    }

    Ok(lhs)
}

fn parse_logical_and_expression(
    context: &mut ParserContext,
    env: &mut Environment,
    tokens: &[Token],
    position: &mut usize,
) -> Result<Box<dyn Expression>, Box<Diagnostic>> {
    let mut lhs = parse_bitwise_or_expression(context, env, tokens, position)?;
    while *position < tokens.len() && tokens[*position].kind == TokenKind::LogicalAnd {
        let expected_type = DataType::Boolean;
        match is_expression_type_equals(env, &lhs, &expected_type) {
            ExprTypeCheckResult::ImplicitCasted(expr) => lhs = expr,
            ExprTypeCheckResult::Error(diagnostic) => return Err(diagnostic),
            ExprTypeCheckResult::NotEqualAndCantImplicitCast => {
                return Err(type_mismatch_error(
                    tokens[*position - 1].location,
                    expected_type,
                    lhs.expr_type(env),
                )
                .as_boxed());
            }
            _ => {}
        }

        // Consume operator
        *position += 1;

        let mut rhs = parse_bitwise_or_expression(context, env, tokens, position)?;
        match is_expression_type_equals(env, &rhs, &expected_type) {
            ExprTypeCheckResult::ImplicitCasted(expr) => rhs = expr,
            ExprTypeCheckResult::Error(diagnostic) => return Err(diagnostic),
            ExprTypeCheckResult::NotEqualAndCantImplicitCast => {
                return Err(type_mismatch_error(
                    tokens[*position].location,
                    expected_type,
                    rhs.expr_type(env),
                )
                .as_boxed());
            }
            _ => {}
        }

        lhs = Box::new(LogicalExpression {
            left: lhs,
            operator: BinaryLogicalOperator::And,
            right: rhs,
        });
    }

    Ok(lhs)
}

fn parse_bitwise_or_expression(
    context: &mut ParserContext,
    env: &mut Environment,
    tokens: &[Token],
    position: &mut usize,
) -> Result<Box<dyn Expression>, Box<Diagnostic>> {
    let lhs = parse_logical_xor_expression(context, env, tokens, position)?;
    if *position < tokens.len() && tokens[*position].kind == TokenKind::BitwiseOr {
        *position += 1;

        if lhs.expr_type(env) != DataType::Integer {
            return Err(type_mismatch_error(
                tokens[*position - 2].location,
                DataType::Integer,
                lhs.expr_type(env),
            )
            .as_boxed());
        }

        let rhs = parse_logical_xor_expression(context, env, tokens, position)?;
        if rhs.expr_type(env) != DataType::Integer {
            return Err(type_mismatch_error(
                tokens[*position].location,
                DataType::Integer,
                lhs.expr_type(env),
            )
            .as_boxed());
        }

        return Ok(Box::new(BitwiseExpression {
            left: lhs,
            operator: BinaryBitwiseOperator::Or,
            right: rhs,
        }));
    }

    Ok(lhs)
}

fn parse_logical_xor_expression(
    context: &mut ParserContext,
    env: &mut Environment,
    tokens: &[Token],
    position: &mut usize,
) -> Result<Box<dyn Expression>, Box<Diagnostic>> {
    let mut lhs = parse_bitwise_and_expression(context, env, tokens, position)?;
    while *position < tokens.len() && tokens[*position].kind == TokenKind::LogicalXor {
        let expected_type = DataType::Boolean;
        match is_expression_type_equals(env, &lhs, &expected_type) {
            ExprTypeCheckResult::ImplicitCasted(expr) => lhs = expr,
            ExprTypeCheckResult::Error(diagnostic) => return Err(diagnostic),
            ExprTypeCheckResult::NotEqualAndCantImplicitCast => {
                return Err(type_mismatch_error(
                    tokens[*position - 1].location,
                    expected_type,
                    lhs.expr_type(env),
                )
                .as_boxed());
            }
            _ => {}
        }

        // Consume operator
        *position += 1;

        let mut rhs = parse_bitwise_and_expression(context, env, tokens, position)?;
        match is_expression_type_equals(env, &rhs, &expected_type) {
            ExprTypeCheckResult::ImplicitCasted(expr) => rhs = expr,
            ExprTypeCheckResult::Error(diagnostic) => return Err(diagnostic),
            ExprTypeCheckResult::NotEqualAndCantImplicitCast => {
                return Err(type_mismatch_error(
                    tokens[*position].location,
                    expected_type,
                    rhs.expr_type(env),
                )
                .as_boxed());
            }
            _ => {}
        }

        lhs = Box::new(LogicalExpression {
            left: lhs,
            operator: BinaryLogicalOperator::Xor,
            right: rhs,
        });
    }

    Ok(lhs)
}

fn parse_bitwise_and_expression(
    context: &mut ParserContext,
    env: &mut Environment,
    tokens: &[Token],
    position: &mut usize,
) -> Result<Box<dyn Expression>, Box<Diagnostic>> {
    let mut lhs = parse_equality_expression(context, env, tokens, position)?;
    if *position < tokens.len() && tokens[*position].kind == TokenKind::BitwiseAnd {
        *position += 1;

        if lhs.expr_type(env) != DataType::Integer {
            return Err(type_mismatch_error(
                tokens[*position - 2].location,
                DataType::Integer,
                lhs.expr_type(env),
            ));
        }

        let rhs = parse_equality_expression(context, env, tokens, position)?;
        if rhs.expr_type(env) != DataType::Integer {
            return Err(type_mismatch_error(
                tokens[*position].location,
                DataType::Integer,
                lhs.expr_type(env),
            ));
        }

        lhs = Box::new(BitwiseExpression {
            left: lhs,
            operator: BinaryBitwiseOperator::And,
            right: rhs,
        });
    }

    Ok(lhs)
}

fn parse_equality_expression(
    context: &mut ParserContext,
    env: &mut Environment,
    tokens: &[Token],
    position: &mut usize,
) -> Result<Box<dyn Expression>, Box<Diagnostic>> {
    let expression = parse_comparison_expression(context, env, tokens, position);
    if expression.is_err() || *position >= tokens.len() {
        return expression;
    }

    let mut lhs = expression.ok().unwrap();

    let operator = &tokens[*position];
    if operator.kind == TokenKind::Equal || operator.kind == TokenKind::BangEqual {
        *position += 1;
        let comparison_operator = if operator.kind == TokenKind::Equal {
            ComparisonOperator::Equal
        } else {
            ComparisonOperator::NotEqual
        };

        let mut rhs = parse_comparison_expression(context, env, tokens, position)?;

        match are_types_equals(env, &lhs, &rhs) {
            TypeCheckResult::Equals => {}
            TypeCheckResult::RightSideCasted(expr) => rhs = expr,
            TypeCheckResult::LeftSideCasted(expr) => lhs = expr,
            TypeCheckResult::NotEqualAndCantImplicitCast => {
                let lhs_type = lhs.expr_type(env);
                let rhs_type = rhs.expr_type(env);
                let diagnostic = Diagnostic::error(&format!(
                    "Can't compare values of different types `{}` and `{}`",
                    lhs_type, rhs_type
                ))
                .with_location(get_safe_location(tokens, *position - 2));

                // Provides help messages if use compare null to non null value
                if lhs_type.is_null() || rhs_type.is_null() {
                    return Err(diagnostic
                        .add_help("Try to use `IS NULL expr` expression")
                        .add_help("Try to use `ISNULL(expr)` function")
                        .as_boxed());
                }

                return Err(diagnostic.as_boxed());
            }
            TypeCheckResult::Error(diagnostic) => {
                return Err(diagnostic
                    .with_location(get_safe_location(tokens, *position - 2))
                    .as_boxed());
            }
        };

        return Ok(Box::new(ComparisonExpression {
            left: lhs,
            operator: comparison_operator,
            right: rhs,
        }));
    }

    Ok(lhs)
}

fn parse_comparison_expression(
    context: &mut ParserContext,
    env: &mut Environment,
    tokens: &[Token],
    position: &mut usize,
) -> Result<Box<dyn Expression>, Box<Diagnostic>> {
    let expression = parse_bitwise_shift_expression(context, env, tokens, position);
    if expression.is_err() || *position >= tokens.len() {
        return expression;
    }

    let mut lhs = expression.ok().unwrap();
    if is_comparison_operator(&tokens[*position]) {
        let operator = &tokens[*position];
        *position += 1;
        let comparison_operator = match operator.kind {
            TokenKind::Greater => ComparisonOperator::Greater,
            TokenKind::GreaterEqual => ComparisonOperator::GreaterEqual,
            TokenKind::Less => ComparisonOperator::Less,
            TokenKind::LessEqual => ComparisonOperator::LessEqual,
            _ => ComparisonOperator::NullSafeEqual,
        };

        let mut rhs = parse_bitwise_shift_expression(context, env, tokens, position)?;
        match are_types_equals(env, &lhs, &rhs) {
            TypeCheckResult::Equals => {}
            TypeCheckResult::RightSideCasted(expr) => rhs = expr,
            TypeCheckResult::LeftSideCasted(expr) => lhs = expr,
            TypeCheckResult::NotEqualAndCantImplicitCast => {
                let lhs_type = lhs.expr_type(env);
                let rhs_type = rhs.expr_type(env);
                let diagnostic = Diagnostic::error(&format!(
                    "Can't compare values of different types `{}` and `{}`",
                    lhs_type, rhs_type
                ))
                .with_location(get_safe_location(tokens, *position - 2));

                // Provides help messages if use compare null to non null value
                if lhs_type.is_null() || rhs_type.is_null() {
                    return Err(diagnostic
                        .add_help("Try to use `IS NULL expr` expression")
                        .add_help("Try to use `ISNULL(expr)` function")
                        .as_boxed());
                }

                return Err(diagnostic.as_boxed());
            }
            TypeCheckResult::Error(diagnostic) => {
                return Err(diagnostic
                    .with_location(get_safe_location(tokens, *position - 2))
                    .as_boxed());
            }
        };

        return Ok(Box::new(ComparisonExpression {
            left: lhs,
            operator: comparison_operator,
            right: rhs,
        }));
    }

    Ok(lhs)
}

fn parse_bitwise_shift_expression(
    context: &mut ParserContext,
    env: &mut Environment,
    tokens: &[Token],
    position: &mut usize,
) -> Result<Box<dyn Expression>, Box<Diagnostic>> {
    let mut lhs = parse_term_expression(context, env, tokens, position)?;

    while *position < tokens.len() && is_bitwise_shift_operator(&tokens[*position]) {
        let operator = &tokens[*position];
        *position += 1;
        let bitwise_operator = if operator.kind == TokenKind::BitwiseRightShift {
            BinaryBitwiseOperator::RightShift
        } else {
            BinaryBitwiseOperator::LeftShift
        };

        let rhs = parse_term_expression(context, env, tokens, position)?;

        // Make sure right and left hand side types are numbers
        if rhs.expr_type(env).is_int() && rhs.expr_type(env) != lhs.expr_type(env) {
            return Err(Diagnostic::error(&format!(
                "Bitwise operators require number types but got `{}` and `{}`",
                lhs.expr_type(env),
                rhs.expr_type(env)
            ))
            .with_location(get_safe_location(tokens, *position - 2))
            .as_boxed());
        }

        lhs = Box::new(BitwiseExpression {
            left: lhs,
            operator: bitwise_operator,
            right: rhs,
        });
    }

    Ok(lhs)
}

fn parse_term_expression(
    context: &mut ParserContext,
    env: &mut Environment,
    tokens: &[Token],
    position: &mut usize,
) -> Result<Box<dyn Expression>, Box<Diagnostic>> {
    let mut lhs = parse_factor_expression(context, env, tokens, position)?;

    while *position < tokens.len() && is_term_operator(&tokens[*position]) {
        let operator = &tokens[*position];
        *position += 1;
        let math_operator = if operator.kind == TokenKind::Plus {
            ArithmeticOperator::Plus
        } else {
            ArithmeticOperator::Minus
        };

        let rhs = parse_factor_expression(context, env, tokens, position)?;

        let lhs_type = lhs.expr_type(env);
        let rhs_type = rhs.expr_type(env);

        // Make sure right and left hand side types are numbers
        if lhs_type.is_number() && rhs_type.is_number() {
            lhs = Box::new(ArithmeticExpression {
                left: lhs,
                operator: math_operator,
                right: rhs,
            });

            continue;
        }

        // Report Error message that suggest to replace `+` operator by `CONCAT` function
        if math_operator == ArithmeticOperator::Plus {
            return Err(Diagnostic::error(&format!(
                "Math operators `+` both sides to be number types but got `{}` and `{}`",
                lhs_type, rhs_type
            ))
            .add_help(
                "You can use `CONCAT(Any, Any, ...Any)` function to concatenate values with different types",
            )
            .with_location(operator.location)
            .as_boxed());
        }

        return Err(Diagnostic::error(&format!(
            "Math operators require number types but got `{}` and `{}`",
            lhs_type, rhs_type
        ))
        .with_location(operator.location)
        .as_boxed());
    }

    Ok(lhs)
}

fn parse_factor_expression(
    context: &mut ParserContext,
    env: &mut Environment,
    tokens: &[Token],
    position: &mut usize,
) -> Result<Box<dyn Expression>, Box<Diagnostic>> {
    let mut lhs = parse_like_expression(context, env, tokens, position)?;
    while *position < tokens.len() && is_factor_operator(&tokens[*position]) {
        let operator = &tokens[*position];
        *position += 1;

        let factor_operator = match operator.kind {
            TokenKind::Star => ArithmeticOperator::Star,
            TokenKind::Slash => ArithmeticOperator::Slash,
            _ => ArithmeticOperator::Modulus,
        };

        let rhs = parse_like_expression(context, env, tokens, position)?;

        let lhs_type = lhs.expr_type(env);
        let rhs_type = rhs.expr_type(env);

        // Make sure right and left hand side types are numbers
        if lhs_type.is_number() && rhs_type.is_number() {
            lhs = Box::new(ArithmeticExpression {
                left: lhs,
                operator: factor_operator,
                right: rhs,
            });
            continue;
        }

        return Err(Diagnostic::error(&format!(
            "Math operators require number types but got `{}` and `{}`",
            lhs_type, rhs_type
        ))
        .with_location(get_safe_location(tokens, *position - 2))
        .as_boxed());
    }

    Ok(lhs)
}

fn parse_like_expression(
    context: &mut ParserContext,
    env: &mut Environment,
    tokens: &[Token],
    position: &mut usize,
) -> Result<Box<dyn Expression>, Box<Diagnostic>> {
    let expression = parse_glob_expression(context, env, tokens, position);
    if expression.is_err() || *position >= tokens.len() {
        return expression;
    }

    let lhs = expression.ok().unwrap();
    if tokens[*position].kind == TokenKind::Like {
        let location = tokens[*position].location;
        *position += 1;

        if !lhs.expr_type(env).is_text() {
            return Err(Diagnostic::error(&format!(
                "Expect `LIKE` left hand side to be `TEXT` but got {}",
                lhs.expr_type(env)
            ))
            .with_location(location)
            .as_boxed());
        }

        let pattern = parse_glob_expression(context, env, tokens, position)?;
        if !pattern.expr_type(env).is_text() {
            return Err(Diagnostic::error(&format!(
                "Expect `LIKE` right hand side to be `TEXT` but got {}",
                pattern.expr_type(env)
            ))
            .with_location(location)
            .as_boxed());
        }

        return Ok(Box::new(LikeExpression {
            input: lhs,
            pattern,
        }));
    }

    Ok(lhs)
}

fn parse_glob_expression(
    context: &mut ParserContext,
    env: &mut Environment,
    tokens: &[Token],
    position: &mut usize,
) -> Result<Box<dyn Expression>, Box<Diagnostic>> {
    let expression = parse_index_or_slice_expression(context, env, tokens, position);
    if expression.is_err() || *position >= tokens.len() {
        return expression;
    }

    let lhs = expression.ok().unwrap();
    if tokens[*position].kind == TokenKind::Glob {
        let location = tokens[*position].location;
        *position += 1;

        if !lhs.expr_type(env).is_text() {
            return Err(Diagnostic::error(&format!(
                "Expect `GLOB` left hand side to be `TEXT` but got {}",
                lhs.expr_type(env)
            ))
            .with_location(location)
            .as_boxed());
        }

        let pattern = parse_index_or_slice_expression(context, env, tokens, position)?;
        if !pattern.expr_type(env).is_text() {
            return Err(Diagnostic::error(&format!(
                "Expect `GLOB` right hand side to be `TEXT` but got {}",
                pattern.expr_type(env)
            ))
            .with_location(location)
            .as_boxed());
        }

        return Ok(Box::new(GlobExpression {
            input: lhs,
            pattern,
        }));
    }

    Ok(lhs)
}

fn parse_index_or_slice_expression(
    context: &mut ParserContext,
    env: &mut Environment,
    tokens: &[Token],
    position: &mut usize,
) -> Result<Box<dyn Expression>, Box<Diagnostic>> {
    let mut expression = parse_prefix_unary_expression(context, env, tokens, position)?;

    while *position < tokens.len() && tokens[*position].kind == TokenKind::LeftBracket {
        // Consume Left Bracket `[`
        *position += 1;

        let expression_type = expression.expr_type(env);

        if !expression_type.is_array() {
            return Err(
                Diagnostic::error("Expect Right side of index expression to be Array")
                    .with_location(get_safe_location(tokens, *position))
                    .as_boxed(),
            );
        }

        // Make sure right hand side is an Array
        let rhs_type = expression.expr_type(env);
        if !rhs_type.is_array() {
            return Err(Diagnostic::error(&format!(
                "Expect right side of index expression to be Array but got {}",
                rhs_type
            ))
            .with_location(get_safe_location(tokens, *position))
            .as_boxed());
        }

        // Slice with end only range [:end]
        if *position < tokens.len() && tokens[*position].kind == TokenKind::Colon {
            // Consume Colon `:`
            *position += 1;

            // In case the user use default slice start and end, we can ignore the slice expression
            // and return array or any kind of expression value directly
            if *position < tokens.len() && tokens[*position].kind == TokenKind::RightBracket {
                // Consume right bracket `]`
                *position += 1;
                return Ok(expression);
            }

            let slice_end = parse_prefix_unary_expression(context, env, tokens, position)?;
            let end_type = slice_end.expr_type(env);
            if !end_type.is_int() {
                return Err(Diagnostic::error(&format!(
                    "Expect Slice expression end value to be Int but got {}",
                    end_type
                ))
                .with_location(get_safe_location(tokens, *position))
                .as_boxed());
            }

            if *position < tokens.len() && tokens[*position].kind == TokenKind::RightBracket {
                // Consume Right Bracket `]`
                *position += 1;
            } else {
                return Err(Diagnostic::error("Expect `]` After Slice expression")
                    .with_location(get_safe_location(tokens, *position))
                    .as_boxed());
            }

            expression = Box::new(SliceExpression {
                collection: expression,
                start: None,
                end: Some(slice_end),
            });
            continue;
        }

        let index = parse_prefix_unary_expression(context, env, tokens, position)?;

        // Slice Expression with Start and End range [start:end]
        if *position < tokens.len() && tokens[*position].kind == TokenKind::Colon {
            // Consume Colon `:`
            *position += 1;

            let start_type = index.expr_type(env);
            if !start_type.is_int() {
                return Err(Diagnostic::error(&format!(
                    "Expect Slice expression start value to be Int but got {}",
                    start_type
                ))
                .with_location(get_safe_location(tokens, *position))
                .as_boxed());
            }

            // Slice with start only range [start:]
            if *position < tokens.len() && tokens[*position].kind == TokenKind::RightBracket {
                // Consume Right Bracket `]`
                *position += 1;

                expression = Box::new(SliceExpression {
                    collection: expression,
                    start: Some(index),
                    end: None,
                });
                continue;
            }

            let slice_end = parse_prefix_unary_expression(context, env, tokens, position)?;
            let end_type = slice_end.expr_type(env);
            if !end_type.is_int() {
                return Err(Diagnostic::error(&format!(
                    "Expect Slice expression end value to be Int but got {}",
                    end_type
                ))
                .with_location(get_safe_location(tokens, *position))
                .as_boxed());
            }

            if *position < tokens.len() && tokens[*position].kind == TokenKind::RightBracket {
                // Consume Right Bracket `]`
                *position += 1;
            } else {
                return Err(Diagnostic::error("Expect `]` After Slice expression")
                    .with_location(get_safe_location(tokens, *position))
                    .as_boxed());
            }

            expression = Box::new(SliceExpression {
                collection: expression,
                start: Some(index),
                end: Some(slice_end),
            });
            continue;
        }

        // Index Expression
        // Make sure index is integer type
        let index_type = index.expr_type(env);
        if !index_type.is_int() {
            return Err(Diagnostic::error(&format!(
                "Expect Index value to be Int but got {}",
                index_type
            ))
            .with_location(get_safe_location(tokens, *position))
            .as_boxed());
        }

        let array_element_type = match expression_type {
            DataType::Array(element_type) => *element_type,
            _ => DataType::Any,
        };

        if *position < tokens.len() && tokens[*position].kind == TokenKind::RightBracket {
            // Consume Left Right `]`
            *position += 1;
        } else {
            return Err(Diagnostic::error("Expect `]` after index expression")
                .with_location(get_safe_location(tokens, *position))
                .as_boxed());
        }

        expression = Box::new(IndexExpression {
            collection: expression,
            element_type: array_element_type,
            index,
        });
    }

    Ok(expression)
}

fn parse_prefix_unary_expression(
    context: &mut ParserContext,
    env: &mut Environment,
    tokens: &[Token],
    position: &mut usize,
) -> Result<Box<dyn Expression>, Box<Diagnostic>> {
    if *position < tokens.len() && is_prefix_unary_operator(&tokens[*position]) {
        let op_location = tokens[*position].location;
        let op = if tokens[*position].kind == TokenKind::Bang {
            PrefixUnaryOperator::Bang
        } else if tokens[*position].kind == TokenKind::Minus {
            PrefixUnaryOperator::Minus
        } else {
            PrefixUnaryOperator::Not
        };

        *position += 1;

        let mut rhs = parse_prefix_unary_expression(context, env, tokens, position)?;
        match type_check_prefix_unary(env, &rhs, &op, op_location) {
            ExprTypeCheckResult::NotEqualAndCantImplicitCast => {
                return Err(type_mismatch_error(
                    op_location,
                    prefix_unary_expected_type(&op),
                    rhs.expr_type(env),
                ))
            }
            ExprTypeCheckResult::Error(diagnostic) => return Err(diagnostic),
            ExprTypeCheckResult::ImplicitCasted(new_expr) => rhs = new_expr,
            _ => {}
        }

        return Ok(Box::new(UnaryExpression { right: rhs, op }));
    }

    parse_function_call_expression(context, env, tokens, position)
}

fn parse_function_call_expression(
    context: &mut ParserContext,
    env: &mut Environment,
    tokens: &[Token],
    position: &mut usize,
) -> Result<Box<dyn Expression>, Box<Diagnostic>> {
    if *position < tokens.len() && tokens[*position].kind == TokenKind::Symbol {
        let symbol_token = &tokens[*position];
        if *position + 1 < tokens.len() && tokens[*position + 1].kind == TokenKind::LeftParen {
            let function_name = &symbol_token.literal;
            let function_name_location = symbol_token.location;

            // Consume function name
            *position += 1;

            // Check if this function is a Standard library functions
            if env.is_std_function(function_name.as_str()) {
                let mut arguments = parse_arguments_expressions(context, env, tokens, position)?;
                let signature = env.std_signature(function_name.as_str()).unwrap();

                check_function_call_arguments(
                    env,
                    &mut arguments,
                    &signature.parameters,
                    function_name.to_string(),
                    function_name_location,
                )?;

                // Register function name with return type
                let return_type = resolve_call_expression_return_type(env, signature, &arguments);
                env.define(function_name.to_string(), return_type.clone());

                return Ok(Box::new(CallExpression {
                    function_name: function_name.to_string(),
                    arguments,
                    return_type,
                }));
            }

            // Check if this function is an Aggregation functions
            if env.is_aggregation_function(function_name.as_str()) {
                let aggregations_count_before = context.aggregations.len();
                let mut arguments = parse_arguments_expressions(context, env, tokens, position)?;
                let has_aggregations = context.aggregations.len() != aggregations_count_before;

                // Prevent calling aggregation function with aggregation values as argument
                if has_aggregations {
                    return Err(Diagnostic::error(
                        "Aggregated values can't as used for aggregation function argument",
                    )
                    .with_location(function_name_location)
                    .as_boxed());
                }

                let signature = env.aggregation_signature(function_name.as_str()).unwrap();

                // Perform type checking and implicit casting if needed for function arguments
                check_function_call_arguments(
                    env,
                    &mut arguments,
                    &signature.parameters,
                    function_name.to_string(),
                    function_name_location,
                )?;

                let column_name = context.generate_column_name();
                context.hidden_selections.push(column_name.to_string());

                // Register aggregation generated name with return type
                let return_type = resolve_call_expression_return_type(env, signature, &arguments);
                env.define(column_name.to_string(), return_type);

                context.aggregations.insert(
                    column_name.clone(),
                    AggregateValue::Function(function_name.to_string(), arguments),
                );

                // Return a Symbol that reference to the aggregation function generated name
                return Ok(Box::new(SymbolExpression { value: column_name }));
            }

            // Report that this function name is not standard or aggregation
            return Err(Diagnostic::error("No such function name")
                .add_help(&format!(
                    "Function `{}` is not an Aggregation or Standard library function name",
                    function_name,
                ))
                .with_location(function_name_location)
                .as_boxed());
        }
    }

    parse_primary_expression(context, env, tokens, position)
}

fn parse_arguments_expressions(
    context: &mut ParserContext,
    env: &mut Environment,
    tokens: &[Token],
    position: &mut usize,
) -> Result<Vec<Box<dyn Expression>>, Box<Diagnostic>> {
    let mut arguments: Vec<Box<dyn Expression>> = vec![];
    if consume_kind(tokens, *position, TokenKind::LeftParen).is_ok() {
        *position += 1;

        while *position < tokens.len() && tokens[*position].kind != TokenKind::RightParen {
            let argument = parse_expression(context, env, tokens, position)?;
            if let Some(argument_literal) = expression_literal(&argument) {
                context.hidden_selections.push(argument_literal);
            }

            arguments.push(argument);

            if *position < tokens.len() && tokens[*position].kind == TokenKind::Comma {
                *position += 1;
            } else {
                break;
            }
        }

        if consume_kind(tokens, *position, TokenKind::RightParen).is_err() {
            return Err(
                Diagnostic::error("Expect `)` after function call arguments")
                    .add_help("Try to add ')' at the end of function call, after arguments")
                    .with_location(get_safe_location(tokens, *position))
                    .as_boxed(),
            );
        }

        *position += 1;
    }
    Ok(arguments)
}

fn parse_primary_expression(
    context: &mut ParserContext,
    env: &mut Environment,
    tokens: &[Token],
    position: &mut usize,
) -> Result<Box<dyn Expression>, Box<Diagnostic>> {
    if *position >= tokens.len() {
        return Err(un_expected_expression_error(tokens, position));
    }

    match tokens[*position].kind {
        TokenKind::Integer => parse_const_integer_expression(tokens, position),
        TokenKind::Float => parse_const_float_expression(tokens, position),
        TokenKind::Symbol => parse_symbol_expression(context, env, tokens, position),
        TokenKind::Array => parse_array_value_expression(context, env, tokens, position),
        TokenKind::LeftBracket => parse_array_value_expression(context, env, tokens, position),
        TokenKind::LeftParen => parse_group_expression(context, env, tokens, position),
        TokenKind::Case => parse_case_expression(context, env, tokens, position),
        TokenKind::String => {
            *position += 1;
            Ok(Box::new(StringExpression {
                value: tokens[*position - 1].literal.to_string(),
                value_type: StringValueType::Text,
            }))
        }
        TokenKind::GlobalVariable => {
            let name = tokens[*position].literal.to_string();
            *position += 1;
            Ok(Box::new(GlobalVariableExpression { name }))
        }
        TokenKind::True => {
            *position += 1;
            Ok(Box::new(BooleanExpression { is_true: true }))
        }
        TokenKind::False => {
            *position += 1;
            Ok(Box::new(BooleanExpression { is_true: false }))
        }
        TokenKind::Null => {
            *position += 1;
            Ok(Box::new(NullExpression {}))
        }
        _ => Err(un_expected_expression_error(tokens, position)),
    }
}

fn parse_const_integer_expression(
    tokens: &[Token],
    position: &mut usize,
) -> Result<Box<dyn Expression>, Box<Diagnostic>> {
    if let Ok(integer) = tokens[*position].literal.parse::<i64>() {
        *position += 1;
        let value = Value::Integer(integer);
        return Ok(Box::new(NumberExpression { value }));
    }

    Err(Diagnostic::error("Too big Integer value")
        .add_help("Try to use smaller value")
        .add_note(&format!(
            "Integer value must be between {} and {}",
            i64::MIN,
            i64::MAX
        ))
        .with_location(tokens[*position].location)
        .as_boxed())
}

fn parse_const_float_expression(
    tokens: &[Token],
    position: &mut usize,
) -> Result<Box<dyn Expression>, Box<Diagnostic>> {
    if let Ok(float) = tokens[*position].literal.parse::<f64>() {
        *position += 1;
        let value = Value::Float(float);
        return Ok(Box::new(NumberExpression { value }));
    }

    Err(Diagnostic::error("Too big Float value")
        .add_help("Try to use smaller value")
        .add_note(&format!(
            "Float value must be between {} and {}",
            f64::MIN,
            f64::MAX
        ))
        .with_location(tokens[*position].location)
        .as_boxed())
}

fn parse_symbol_expression(
    context: &mut ParserContext,
    env: &mut Environment,
    tokens: &[Token],
    position: &mut usize,
) -> Result<Box<dyn Expression>, Box<Diagnostic>> {
    let mut value = tokens[*position].literal.to_string();
    let location = tokens[*position].location;

    // Collect projections only inside select statement
    if !context.has_select_statement {
        context.projection_names.push(value.to_string());
        context.projection_locations.push(location);
    }

    *position += 1;

    if context.has_select_statement {
        // Replace name by alias if it used after select statement
        // This workaround will help to execute query like
        // SELECT commit_count as cc from branches where commit_count > 1
        if context.name_alias_table.contains_key(&value) {
            value = context.name_alias_table[&value].to_string();
        }

        if !env.scopes.contains_key(&value) {
            return Err(Diagnostic::error("Unresolved column or variable name")
                .add_help("Please check schema from docs website or SHOW query")
                .with_location(tokens[*position].location)
                .as_boxed());
        }

        if !context.selected_fields.contains(&value) {
            context.hidden_selections.push(value.to_string());
        }
    }

    Ok(Box::new(SymbolExpression { value }))
}

fn parse_array_value_expression(
    context: &mut ParserContext,
    env: &mut Environment,
    tokens: &[Token],
    position: &mut usize,
) -> Result<Box<dyn Expression>, Box<Diagnostic>> {
    // Consume the Optional Array keyword
    if *position < tokens.len() && tokens[*position].kind == TokenKind::Array {
        // Consume Array keyword
        *position += 1;

        // Make sure Array keyword followed by [
        if *position >= tokens.len() || tokens[*position].kind != TokenKind::LeftBracket {
            return Err(Diagnostic::error("Expect `[` after `ARRAY` keyword")
                .with_location(get_safe_location(tokens, *position))
                .add_help("Try to add '[' after `ARRAY` keyword")
                .as_boxed());
        }
    }

    // Consume Left Bracket `[`
    *position += 1;

    // Parse array values
    let mut array_values: Vec<Box<dyn Expression>> = vec![];
    let mut array_data_type = DataType::Any;
    while *position < tokens.len() && tokens[*position].kind != TokenKind::RightBracket {
        let value = parse_expression(context, env, tokens, position)?;
        let value_type = value.expr_type(env);
        if value_type != array_data_type {
            return Err(Diagnostic::error("Expect Array values to have same types")
                .with_location(get_safe_location(tokens, *position))
                .as_boxed());
        }

        array_data_type = value_type;
        array_values.push(value);

        if *position < tokens.len() && tokens[*position].kind == TokenKind::Comma {
            *position += 1;
        } else {
            break;
        }
    }

    // Make sure Array values end with by ]
    if *position >= tokens.len() || tokens[*position].kind != TokenKind::RightBracket {
        return Err(Diagnostic::error("Expect `]` at the end of array values")
            .with_location(get_safe_location(tokens, *position))
            .add_help("Try to add ']' at the end of array values")
            .as_boxed());
    }

    // Consume Right Bracket `]`
    *position += 1;

    Ok(Box::new(ArrayExpression {
        values: array_values,
        element_type: array_data_type,
    }))
}

fn parse_group_expression(
    context: &mut ParserContext,
    env: &mut Environment,
    tokens: &[Token],
    position: &mut usize,
) -> Result<Box<dyn Expression>, Box<Diagnostic>> {
    *position += 1;
    let expression = parse_expression(context, env, tokens, position)?;
    if tokens[*position].kind != TokenKind::RightParen {
        return Err(Diagnostic::error("Expect `)` to end group expression")
            .with_location(get_safe_location(tokens, *position))
            .add_help("Try to add ')' at the end of group expression")
            .as_boxed());
    }
    *position += 1;
    Ok(expression)
}

fn parse_case_expression(
    context: &mut ParserContext,
    env: &mut Environment,
    tokens: &[Token],
    position: &mut usize,
) -> Result<Box<dyn Expression>, Box<Diagnostic>> {
    let mut conditions: Vec<Box<dyn Expression>> = vec![];
    let mut values: Vec<Box<dyn Expression>> = vec![];
    let mut default_value: Option<Box<dyn Expression>> = None;

    // Consume `case` keyword
    let case_location = tokens[*position].location;
    *position += 1;

    let mut has_else_branch = false;

    while *position < tokens.len() && tokens[*position].kind != TokenKind::End {
        // Else branch
        if tokens[*position].kind == TokenKind::Else {
            if has_else_branch {
                return Err(
                    Diagnostic::error("This `CASE` expression already has else branch")
                        .add_note("`CASE` expression can has only one `ELSE` branch")
                        .with_location(get_safe_location(tokens, *position))
                        .as_boxed(),
                );
            }

            // Consume `ELSE` keyword
            *position += 1;

            let default_value_expr = parse_expression(context, env, tokens, position)?;
            default_value = Some(default_value_expr);
            has_else_branch = true;
            continue;
        }

        // Check if current token kind is `WHEN` keyword
        let when_result = consume_kind(tokens, *position, TokenKind::When);
        if when_result.is_err() {
            return Err(Diagnostic::error("Expect `when` before case condition")
                .add_help("Try to add `WHEN` keyword before any condition")
                .with_location(get_safe_location(tokens, *position))
                .as_boxed());
        }

        // Consume `WHEN` keyword
        *position += 1;

        let condition = parse_expression(context, env, tokens, position)?;
        if condition.expr_type(env) != DataType::Boolean {
            return Err(Diagnostic::error("Case condition must be a boolean type")
                .with_location(get_safe_location(tokens, *position))
                .as_boxed());
        }

        conditions.push(condition);

        let then_result = consume_kind(tokens, *position, TokenKind::Then);
        if then_result.is_err() {
            return Err(Diagnostic::error("Expect `THEN` after case condition")
                .with_location(get_safe_location(tokens, *position))
                .as_boxed());
        }

        // Consume then keyword
        *position += 1;

        let expression = parse_expression(context, env, tokens, position)?;
        values.push(expression);
    }

    // Make sure case expression has at least else branch
    if conditions.is_empty() && !has_else_branch {
        return Err(
            Diagnostic::error("Case expression must has at least else branch")
                .with_location(get_safe_location(tokens, *position))
                .as_boxed(),
        );
    }

    // Make sure case expression end with END keyword
    if *position >= tokens.len() || tokens[*position].kind != TokenKind::End {
        return Err(Diagnostic::error("Expect `END` after case branches")
            .with_location(get_safe_location(tokens, *position))
            .as_boxed());
    }

    // Consume end
    *position += 1;

    // Make sure this case expression has else branch
    if !has_else_branch {
        return Err(Diagnostic::error("Case expression must has else branch")
            .with_location(get_safe_location(tokens, *position))
            .as_boxed());
    }

    // Assert that all values has the same type
    let values_type: DataType = values[0].expr_type(env);
    for (i, value) in values.iter().enumerate().skip(1) {
        if values_type != value.expr_type(env) {
            return Err(Diagnostic::error(&format!(
                "Case value in branch {} has different type than the last branch",
                i + 1
            ))
            .add_note("All values in `CASE` expression must has the same Type")
            .with_location(case_location)
            .as_boxed());
        }
    }

    Ok(Box::new(CaseExpression {
        conditions,
        values,
        default_value,
        values_type,
    }))
}

fn un_expected_statement_error(tokens: &[Token], position: &mut usize) -> Box<Diagnostic> {
    let token: &Token = &tokens[*position];
    let location = token.location;

    // Query starts with invalid statement
    if location.start == 0 {
        return Diagnostic::error("Unexpected statement")
            .add_help("Expect query to start with `SELECT` or `SET` keyword")
            .with_location(location)
            .as_boxed();
    }

    // General un expected statement error
    Diagnostic::error("Unexpected statement")
        .with_location(location)
        .as_boxed()
}

fn un_expected_expression_error(tokens: &[Token], position: &usize) -> Box<Diagnostic> {
    let location = get_safe_location(tokens, *position);

    if *position == 0 || *position >= tokens.len() {
        return Diagnostic::error("Can't complete parsing this expression")
            .with_location(location)
            .as_boxed();
    }

    let current = &tokens[*position];
    let previous = &tokens[*position - 1];

    // Make sure `ASC` and `DESC` are used in ORDER BY statement
    if current.kind == TokenKind::Ascending || current.kind == TokenKind::Descending {
        return Diagnostic::error("`ASC` and `DESC` must be used in `ORDER BY` statement")
            .with_location(location)
            .as_boxed();
    }

    // Similar to SQL just `=` is used for equality comparisons
    if previous.kind == TokenKind::Equal && current.kind == TokenKind::Equal {
        return Diagnostic::error("Unexpected `==`, Just use `=` to check equality")
            .add_help("Try to remove the extra `=`")
            .with_location(location)
            .as_boxed();
    }

    // `> =` the user may mean to write `>=`
    if previous.kind == TokenKind::Greater && current.kind == TokenKind::Equal {
        return Diagnostic::error("Unexpected `> =`, do you mean `>=`?")
            .add_help("Try to remove space between `> =`")
            .with_location(location)
            .as_boxed();
    }

    // `< =` the user may mean to write `<=`
    if previous.kind == TokenKind::Less && current.kind == TokenKind::Equal {
        return Diagnostic::error("Unexpected `< =`, do you mean `<=`?")
            .add_help("Try to remove space between `< =`")
            .with_location(location)
            .as_boxed();
    }

    // `> >` the user may mean to write '>>'
    if previous.kind == TokenKind::Greater && current.kind == TokenKind::Greater {
        return Diagnostic::error("Unexpected `> >`, do you mean `>>`?")
            .add_help("Try to remove space between `> >`")
            .with_location(location)
            .as_boxed();
    }

    // `< <` the user may mean to write `<<`
    if previous.kind == TokenKind::Less && current.kind == TokenKind::Less {
        return Diagnostic::error("Unexpected `< <`, do you mean `<<`?")
            .add_help("Try to remove space between `< <`")
            .with_location(location)
            .as_boxed();
    }

    // `< >` the user may mean to write `<>`
    if previous.kind == TokenKind::Less && current.kind == TokenKind::Greater {
        return Diagnostic::error("Unexpected `< >`, do you mean `<>`?")
            .add_help("Try to remove space between `< >`")
            .with_location(location)
            .as_boxed();
    }

    // `<= >` the user may mean to write `<=>`
    if previous.kind == TokenKind::LessEqual && current.kind == TokenKind::Greater {
        return Diagnostic::error("Unexpected `<= >`, do you mean `<=>`?")
            .add_help("Try to remove space between `<= >`")
            .with_location(location)
            .as_boxed();
    }

    // Default error message
    Diagnostic::error("Can't complete parsing this expression")
        .with_location(location)
        .as_boxed()
}

/// Report error message for extra content after the end of current statement
fn un_expected_content_after_correct_statement(
    statement_name: &str,
    tokens: &[Token],
    position: &mut usize,
) -> Box<Diagnostic> {
    let error_message = &format!(
        "Unexpected content after the end of `{}` statement",
        statement_name.to_uppercase()
    );

    // The range of extra content
    let location_of_extra_content = Location {
        start: tokens[*position].location.start,
        end: tokens[tokens.len() - 1].location.end,
    };

    Diagnostic::error(error_message)
        .add_help("Try to check if statement keyword is missing")
        .add_help("Try remove un expected extra content")
        .with_location(location_of_extra_content)
        .as_boxed()
}

#[inline(always)]
#[allow(clippy::borrowed_box)]
fn expression_literal(expression: &Box<dyn Expression>) -> Option<String> {
    if let Some(symbol) = expression.as_any().downcast_ref::<SymbolExpression>() {
        return Some(symbol.value.to_string());
    }
    None
}

#[inline(always)]
fn register_current_table_fields_types(env: &mut Environment, table_name: &str) {
    let table_fields_names = &env.schema.tables_fields_names[table_name].clone();
    for field_name in table_fields_names {
        let field_type = env.schema.tables_fields_types[field_name].clone();
        env.define(field_name.to_string(), field_type);
    }
}

#[inline(always)]
fn select_all_table_fields(
    env: &mut Environment,
    table_name: &[String],
    selected_fields: &mut Vec<String>,
    fields_names: &mut Vec<String>,
) {
    let mut tables_columns: Vec<&str> = vec![];
    for table in table_name {
        let columns = env.schema.tables_fields_names.get(table.as_str()).unwrap();
        for column in columns {
            tables_columns.push(column);
        }
    }

    for field in tables_columns {
        if !fields_names.contains(&field.to_string()) {
            fields_names.push(field.to_string());
            selected_fields.push(field.to_string());
        }
    }
}

#[inline(always)]
fn consume_kind(tokens: &[Token], position: usize, kind: TokenKind) -> Result<&Token, ()> {
    if position < tokens.len() && tokens[position].kind == kind {
        return Ok(&tokens[position]);
    }
    Err(())
}

#[inline(always)]
fn get_safe_location(tokens: &[Token], position: usize) -> Location {
    if position < tokens.len() {
        return tokens[position].location;
    }
    tokens[tokens.len() - 1].location
}

#[inline(always)]
fn is_assignment_operator(token: &Token) -> bool {
    token.kind == TokenKind::Equal || token.kind == TokenKind::ColonEqual
}

#[inline(always)]
fn is_term_operator(token: &Token) -> bool {
    token.kind == TokenKind::Plus || token.kind == TokenKind::Minus
}

#[inline(always)]
fn is_bitwise_shift_operator(token: &Token) -> bool {
    token.kind == TokenKind::BitwiseLeftShift || token.kind == TokenKind::BitwiseRightShift
}

#[inline(always)]
fn is_prefix_unary_operator(token: &Token) -> bool {
    token.kind == TokenKind::Bang
        || token.kind == TokenKind::Minus
        || token.kind == TokenKind::BitwiseNot
}

#[inline(always)]
fn is_comparison_operator(token: &Token) -> bool {
    token.kind == TokenKind::Greater
        || token.kind == TokenKind::GreaterEqual
        || token.kind == TokenKind::Less
        || token.kind == TokenKind::LessEqual
        || token.kind == TokenKind::NullSafeEqual
}

#[inline(always)]
fn is_factor_operator(token: &Token) -> bool {
    token.kind == TokenKind::Star
        || token.kind == TokenKind::Slash
        || token.kind == TokenKind::Percentage
}

#[inline(always)]
fn is_join_token(token: &Token) -> bool {
    token.kind == TokenKind::Join
        || token.kind == TokenKind::Left
        || token.kind == TokenKind::Right
        || token.kind == TokenKind::Cross
        || token.kind == TokenKind::Inner
}

#[inline(always)]
fn is_asc_or_desc(token: &Token) -> bool {
    token.kind == TokenKind::Ascending || token.kind == TokenKind::Descending
}
