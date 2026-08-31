// tree-sitter-duckdb — a tree-sitter grammar derived from DuckDB's PEG
// grammar (the bible: upstream/statements/*.gram, pinned in
// upstream/COMMIT). Structural node names mirror the PEG rule names,
// snake_cased (SelectStatement -> select_statement, StarExpression ->
// star_expression); the 16-level stratified expression ladder collapses
// into precedence-annotated binary rules, one prec number per PEG LEVEL
// comment. Keywords follow DuckDB's own discipline: only structurally
// required keywords are tokens (case-insensitive, aliased to their
// uppercase spelling); unreserved keywords parse as identifiers, exactly
// as the PEG's ColId does. Deviations from a line-by-line transliteration
// are deliberate — PEG resolves ambiguity by choice order, tree-sitter
// by GLR — and each is noted where it happens.

// A case-insensitive keyword, shown in the tree as its uppercase self.
function kw(word) {
  const pattern = word
    .split('')
    .map((c) => (/[a-z]/.test(c) ? `[${c}${c.toUpperCase()}]` : c))
    .join('');
  return alias(token(prec(1, new RegExp(pattern))), word.toUpperCase());
}

// The PEG's List(D) macro: D (',' D)* ','? — trailing comma allowed.
function list(rule) {
  return seq(rule, repeat(seq(',', rule)), optional(','));
}

function parens(...rules) {
  return seq('(', ...rules, ')');
}

// PEG expression LEVEL numbers, used directly as precedences.
const PREC = {
  LAMBDA: 1,
  OR: 2,
  AND: 3,
  NOT: 4,
  IS: 5,
  COMPARE: 6,
  BETWEEN_IN_LIKE: 7,
  OTHER: 8,
  BITWISE: 9,
  ADD: 10,
  MUL: 11,
  EXP: 12,
  COLLATE: 13,
  AT_TIME_ZONE: 14,
  UNARY: 15,
  INDIRECTION: 16,
};

module.exports = grammar({
  name: 'duckdb',

  extras: ($) => [/[ \t\r\n]/, $.comment, $.block_comment],

  word: ($) => $.plain_identifier,

  conflicts: ($) => [
    // identifier '.' — a bare-identifier expression about to take a
    // field access, a qualified function/type name, or a star qualifier
    // (t.*). The PEG settles these by choice order; GLR carries all
    // three until '(' , '*' , a string, or the end disambiguates.
    [$._primary_expression, $.qualified_name],
    // `x = y NOT …` — NOT may start a tighter NOT IN / NOT LIKE / NOT
    // BETWEEN (shift) or the looser NOT NULL postfix (reduce); only the
    // token after NOT decides. GLR forks; the dead branch collapses one
    // token later.
    [$.binary_expression, $.is_expression, $.between_expression, $.in_expression, $.like_expression],
  ],

  rules: {
    // common.gram: Program <- TopLevelStatement*; statements separated
    // by one-or-more semicolons, final semicolon optional.
    program: ($) => seq(repeat(seq(optional($._statement), ';')), optional($._statement)),

    _statement: ($) =>
      choice(
        $.select_statement,
        $.insert_statement,
        $.update_statement,
        $.delete_statement,
        $.truncate_statement,
        $.create_statement,
        $.drop_statement,
        $.attach_statement,
        $.detach_statement,
        $.use_statement,
        $.set_statement,
        $.reset_statement,
        $.pragma_statement,
        $.call_statement,
        $.explain_statement,
        $.transaction_statement,
        $.copy_statement,
        $.load_statement,
        $.install_statement,
        $.execute_statement,
        $.prepare_statement,
        $.deallocate_statement,
        $.vacuum_statement,
        $.checkpoint_statement,
        $.analyze_statement,
        $.export_statement,
        $.import_statement,
        $.expression_statement
      ),

    // ============================= SELECT =============================
    // select.gram: SelectStatementInternal <- WithClause? SelectSetOpChain
    // ResultModifiers?. The set-op chain (UNION/EXCEPT looser than
    // INTERSECT) becomes two precedence levels of a left-assoc rule.
    select_statement: ($) =>
      prec.right(
        seq(
          optional($.with_clause),
          $._select_set_expression,
          optional($.order_by_clause),
          optional($._limit_offset)
        )
      ),

    _select_set_expression: ($) =>
      choice($.set_operation, $.intersect_operation, $._select_atom),

    set_operation: ($) =>
      prec.left(
        1,
        seq(
          $._select_set_expression,
          field('operation', choice(kw('union'), kw('except'))),
          optional(choice(kw('all'), kw('distinct'))),
          optional(seq(kw('by'), kw('name'))),
          $._select_set_expression
        )
      ),

    intersect_operation: ($) =>
      prec.left(
        2,
        seq(
          $._select_set_expression,
          kw('intersect'),
          optional(choice(kw('all'), kw('distinct'))),
          $._select_set_expression
        )
      ),

    _select_atom: ($) =>
      choice(
        $.simple_select,
        $.values_clause,
        $.table_statement,
        $.describe_statement,
        seq('(', $.select_statement, ')')
      ),

    // select.gram: SelectFrom <- SelectFromClause / FromSelectClause —
    // FROM-first is the second alternative with SELECT optional. The
    // trailing clauses are siblings here (the PEG nests them inside
    // FromClause; flat is equivalent and walks better).
    simple_select: ($) =>
      prec.right(
        seq(
          choice(
            seq($.select_clause, optional($.from_clause)),
            seq($.from_clause, optional($.select_clause))
          ),
          optional($.where_clause),
          optional($.group_by_clause),
          optional($.having_clause),
          optional($.window_clause),
          optional($.qualify_clause),
          optional($.sample_clause)
        )
      ),

    select_clause: ($) =>
      prec.right(
        seq(
          kw('select'),
          optional($.distinct_clause),
          optional($.target_list)
        )
      ),

    distinct_clause: ($) =>
      prec.right(
        choice(
          seq(kw('distinct'), optional(seq(kw('on'), parens(list($._expression))))),
          kw('all')
        )
      ),

    // select.gram AliasedExpression: ColIdExpression (alias: expr) /
    // ExpressionAsCollabel / ExpressionOptIdentifier.
    aliased_expression: ($) =>
      choice(
        prec.right(2, seq(field('alias', $.identifier), ':', $._expression)),
        prec.right(1, seq($._expression, kw('as'), field('alias', $._col_label))),
        prec.right(0, seq($._expression, optional(field('alias', $.identifier))))
      ),

    with_clause: ($) => seq(kw('with'), optional(kw('recursive')), list($.common_table_expression)),

    common_table_expression: ($) =>
      seq(
        field('name', $.identifier),
        optional($.column_aliases),
        optional(seq(kw('using'), kw('key'), parens(list($._expression)))),
        kw('as'),
        optional(seq(optional(kw('not')), kw('materialized'))),
        parens($._statement)
      ),

    from_clause: ($) => prec.right(seq(kw('from'), list($.table_ref))),

    // select.gram: TableRef <- InnerTableRef JoinOrPivot*. Joins are
    // left-recursive here — the tree-sitter idiom for the PEG's trailing
    // repetition.
    table_ref: ($) => prec.right(seq($._inner_table_ref, repeat($.join_clause))),

    _inner_table_ref: ($) =>
      choice(
        $.table_function_call,
        $.table_subquery,
        $.base_table_ref,
        $.values_ref,
        $.parens_table_ref
      ),

    // BaseTableRef <- TableAliasColon? BaseTableName TableAlias?
    // AtClause? SampleClause?
    base_table_ref: ($) =>
      prec.right(
        seq(
          optional($.table_alias_colon),
          field('name', $.qualified_name),
          optional($.table_alias),
          optional($.at_clause),
          optional($.sample_clause)
        )
      ),

    table_subquery: ($) =>
      prec.right(
        1,
        seq(
          optional($.table_alias_colon),
          optional(kw('lateral')),
          parens($.select_statement),
          optional($.table_alias)
        )
      ),

    table_function_call: ($) =>
      prec.right(
        seq(
          optional($.table_alias_colon),
          optional(kw('lateral')),
          field('function', $.qualified_name),
          $.function_arguments,
          optional(seq(kw('with'), kw('ordinality'))),
          optional($.table_alias),
          optional($.sample_clause)
        )
      ),

    values_ref: ($) =>
      prec.right(1, seq(optional($.table_alias_colon), $.values_clause, optional($.table_alias))),

    parens_table_ref: ($) =>
      prec.right(
        seq(
          optional($.table_alias_colon),
          parens($.table_ref),
          optional($.table_alias),
          optional($.sample_clause)
        )
      ),

    // DuckDB's prefix alias: `x: (SELECT ...)`.
    table_alias_colon: ($) => seq(field('alias', $.identifier), token.immediate(':')),

    table_alias: ($) =>
      prec.right(
        choice(
          seq(kw('as'), field('alias', choice($.identifier, $.string_literal)), optional($.column_aliases)),
          seq(field('alias', $.identifier), optional($.column_aliases))
        )
      ),

    at_clause: ($) =>
      seq(kw('at'), parens(choice(kw('version'), kw('timestamp')), '=>', $._expression)),

    join_clause: ($) =>
      prec.right(
        choice(
          seq(
            optional(kw('asof')),
            optional($.join_type),
            kw('join'),
            $.table_ref,
            optional($._join_qualifier)
          ),
          seq($.join_prefix, kw('join'), $._inner_table_ref)
        )
      ),

    join_type: ($) =>
      choice(
        seq(kw('full'), optional(kw('outer'))),
        seq(kw('left'), optional(kw('outer'))),
        seq(kw('right'), optional(kw('outer'))),
        kw('semi'),
        kw('anti'),
        kw('inner')
      ),

    join_prefix: ($) =>
      choice(kw('cross'), seq(kw('natural'), optional($.join_type)), kw('positional')),

    _join_qualifier: ($) => choice($.on_clause, $.using_clause),
    on_clause: ($) => seq(kw('on'), $._expression),
    using_clause: ($) => prec.right(seq(kw('using'), parens(list($.identifier)))),

    where_clause: ($) => seq(kw('where'), $._expression),
    having_clause: ($) => seq(kw('having'), $._expression),
    qualify_clause: ($) => seq(kw('qualify'), $._expression),

    group_by_clause: ($) =>
      prec.right(
        seq(
          kw('group'),
          kw('by'),
          choice(alias(kw('all'), $.group_by_all), list($.group_by_expression))
        )
      ),

    group_by_expression: ($) =>
      choice(
        seq('(', ')'),
        seq(choice(kw('cube'), kw('rollup')), parens(optional(list($._expression)))),
        seq(kw('grouping'), kw('sets'), parens(list($.group_by_expression))),
        $._expression
      ),

    window_clause: ($) => prec.right(seq(kw('window'), list($.window_definition))),
    window_definition: ($) =>
      seq(field('name', $.identifier), kw('as'), $.window_frame_definition),

    order_by_clause: ($) =>
      prec.right(
        seq(
        kw('order'),
        kw('by'),
        choice(
          alias(seq(kw('all'), optional($._order_modifiers)), $.order_by_all),
          list($.order_by_expression)
        )
      )
      ),

    order_by_expression: ($) => seq($._expression, optional($._order_modifiers)),

    _order_modifiers: ($) =>
      choice(
        seq($._desc_or_asc, optional($._nulls_order)),
        seq($._nulls_order, optional($._desc_or_asc)),
      ),
    _desc_or_asc: ($) => choice(kw('desc'), kw('descending'), kw('asc'), kw('ascending')),
    _nulls_order: ($) => seq(kw('nulls'), choice(kw('first'), kw('last'))),

    _limit_offset: ($) =>
      prec.right(
        choice(
          seq($.limit_clause, optional($.offset_clause)),
          seq($.offset_clause, optional($.limit_clause))
        )
      ),

    limit_clause: ($) =>
      seq(
        kw('limit'),
        choice(kw('all'), seq($._expression, optional(choice('%', kw('percent')))))
      ),

    offset_clause: ($) =>
      seq(kw('offset'), $._expression, optional(choice(kw('row'), kw('rows')))),

    sample_clause: ($) =>
      prec.right(
        seq(
          // A single compound token: a lone USING must stay free for join
          // qualifiers (JOIN c USING (id)) — the pair only means sampling.
          choice(
            alias(
              token(seq(/[uU][sS][iI][nN][gG]/, /[ \t\r\n]+/, /[sS][aA][mM][pP][lL][eE]/)),
              'USING SAMPLE'
            ),
            kw('tablesample')
          ),
          choice(
            seq($._sample_count, optional(parens($.identifier, optional(seq(',', $.number_literal))))),
            seq(
              optional($.identifier),
              parens($._sample_count),
              optional(seq(kw('repeatable'), parens($.number_literal)))
            )
          )
        )
      ),

    _sample_count: ($) =>
      prec.right(
        seq(
          choice($.number_literal, $.parameter),
          optional(choice('%', kw('percent'), kw('rows')))
        )
      ),

    values_clause: ($) =>
      prec.right(seq(kw('values'), list($.value_row))),

    table_statement: ($) => seq(kw('table'), $.qualified_name),

    describe_statement: ($) =>
      prec.right(
        seq(
          choice(kw('describe'), kw('show'), kw('summarize')),
          optional(
            choice(
              seq(kw('all'), optional(kw('tables'))),
              seq(kw('tables'), kw('from'), $.qualified_name),
              $.select_statement,
              $.qualified_name,
              $.string_literal
            )
          )
        )
      ),

    // =========================== EXPRESSIONS ==========================
    // expression.gram LEVEL 1..16, collapsed: each tail-repetition level
    // becomes a left-assoc rule at that level's precedence.
    _expression: ($) =>
      choice(
        $.lambda_expression,
        $.binary_expression,
        $.unary_expression,
        $.is_expression,
        $.between_expression,
        $.in_expression,
        $.like_expression,
        $.cast_expression,
        $.subscript_expression,
        $.field_access,
        $.method_call,
        $.postfix_expression,
        $._primary_expression
      ),

    // LEVEL 1.5: SingleArrowPair <- '->' LogicalOrExpression; right-assoc.
    lambda_expression: ($) =>
      prec.right(PREC.LAMBDA, seq($._expression, '->', $._expression)),

    binary_expression: ($) => {
      const table = [
        [PREC.OR, kw('or')],
        [PREC.AND, kw('and')],
        [PREC.COMPARE, choice('=', '==', '!=', '<>', '<', '>', '<=', '>=')],
        [PREC.IS, seq(kw('is'), optional(kw('not')), kw('distinct'), kw('from'))],
        [
          PREC.OTHER,
          choice(
            '->>', '>>=', '<<=', '&&', '@>', '<@', '^@', '||',
            '~~', '~~*', '~~~', '!~~', '!~~*', '~', '~*', '!~', '!~*'
          ),
        ],
        [PREC.BITWISE, choice('&', '|', '<<', '>>')],
        [PREC.ADD, choice('+', '-')],
        [PREC.MUL, choice('*', '/', '//', '%')],
        [PREC.EXP, choice('^', '**')],
        [PREC.COLLATE, kw('collate')],
        [PREC.AT_TIME_ZONE, seq(kw('at'), kw('time'), kw('zone'))],
      ];
      return choice(
        ...table.map(([p, op]) =>
          prec.left(p, seq($._expression, field('operator', op), $._expression))
        )
      );
    },

    // LEVEL 3 NOT and LEVEL 15 prefix operators.
    unary_expression: ($) =>
      choice(
        prec.right(PREC.NOT, seq(kw('not'), $._expression)),
        prec.right(PREC.UNARY, seq(field('operator', choice('-', '+', '~')), $._expression))
      ),

    // LEVEL 4: IS [NOT] TRUE/FALSE/NULL/UNKNOWN, NOTNULL, ISNULL.
    is_expression: ($) =>
      prec.left(
        PREC.IS,
        seq(
          $._expression,
          choice(
            seq(
              kw('is'),
              optional(kw('not')),
              choice(kw('true'), kw('false'), kw('null'), kw('unknown'))
            ),
            seq(kw('not'), kw('null')),
            kw('notnull'),
            kw('isnull')
          )
        )
      ),

    between_expression: ($) =>
      prec.left(
        PREC.BETWEEN_IN_LIKE,
        seq(
          $._expression,
          optional(kw('not')),
          kw('between'),
          $._expression,
          kw('and'),
          $._expression
        )
      ),

    in_expression: ($) =>
      prec.left(
        PREC.BETWEEN_IN_LIKE,
        seq(
          $._expression,
          optional(kw('not')),
          kw('in'),
          choice(parens(list($._expression)), parens($.select_statement), $._expression)
        )
      ),

    like_expression: ($) =>
      prec.left(
        PREC.BETWEEN_IN_LIKE,
        seq(
          $._expression,
          optional(kw('not')),
          choice(kw('like'), kw('ilike'), kw('glob'), seq(kw('similar'), kw('to'))),
          $._expression,
          optional(seq(kw('escape'), $._expression))
        )
      ),

    // LEVEL 16 postfix indirections, each its own named node.
    cast_expression: ($) =>
      choice(
        prec.left(PREC.INDIRECTION, seq($._expression, '::', field('type', $.type))),
        seq(
          choice(kw('cast'), kw('try_cast')),
          parens($._expression, kw('as'), field('type', $.type))
        )
      ),

    subscript_expression: ($) =>
      prec.left(
        PREC.INDIRECTION,
        seq(
          $._expression,
          '[',
          optional($._expression),
          optional(seq(':', optional(choice($._expression, '-')))),
          optional(seq(':', optional($._expression))),
          ']'
        )
      ),

    field_access: ($) =>
      prec.left(PREC.INDIRECTION, seq($._expression, '.', field('field', $._col_label))),

    method_call: ($) =>
      prec.left(
        PREC.INDIRECTION,
        seq(
          $._expression,
          '.',
          field('method', $._col_label),
          $.function_arguments
        )
      ),

    postfix_expression: ($) => prec.left(PREC.INDIRECTION, seq($._expression, '!')),

    _primary_expression: ($) =>
      choice(
        $.parenthesized_expression,
        $.literal,
        $.parameter,
        $.positional_reference,
        $.subquery_expression,
        $.case_expression,
        $.star_expression,
        $.columns_expression,
        $.function_call,
        $.extract_expression,
        $.position_expression,
        $.trim_expression,
        $.legacy_lambda_expression,
        $.list_comprehension,
        $.list_expression,
        $.struct_expression,
        $.map_expression,
        $.interval_literal,
        $.type_literal,
        $.identifier,
        kw('default')
      ),

    parenthesized_expression: ($) => parens(list($._expression)),

    literal: ($) =>
      choice($.string_literal, $.number_literal, kw('true'), kw('false'), kw('null')),

    parameter: ($) =>
      choice(
        seq('?', optional(token.immediate(/[0-9]+/))),
        seq('$', token.immediate(/[0-9]+/)),
        seq('$', token.immediate(/[a-zA-Z_][a-zA-Z0-9_]*/))
      ),

    positional_reference: ($) => seq('#', token.immediate(/[0-9]+/)),

    // The PEG's SubqueryNot is omitted: unary NOT already covers it.
    subquery_expression: ($) =>
      prec(1, seq(optional(kw('exists')), parens($.select_statement))),

    case_expression: ($) =>
      seq(
        kw('case'),
        optional($._expression),
        repeat1(seq(kw('when'), $._expression, kw('then'), $._expression)),
        optional(seq(kw('else'), $._expression)),
        kw('end')
      ),

    // StarExpression <- StarQualifierList? '*' ExcludeList? ReplaceList?
    // RenameList?
    star_expression: ($) =>
      prec.right(
        seq(
          optional(seq(field('qualifier', $.qualified_name), '.')),
          '*',
          optional($.exclude_list),
          optional($.replace_list),
          optional($.rename_list)
        )
      ),

    exclude_list: ($) =>
      seq(
        choice(kw('exclude'), kw('except')),
        choice(parens(list($._dotted_name)), $._dotted_name)
      ),

    replace_list: ($) =>
      seq(
        kw('replace'),
        choice(parens(list($.replace_entry)), $.replace_entry)
      ),

    replace_entry: ($) => seq($._expression, kw('as'), $._dotted_name),

    rename_list: ($) =>
      seq(
        kw('rename'),
        choice(parens(list($.rename_entry)), $.rename_entry)
      ),

    rename_entry: ($) => seq($._dotted_name, kw('as'), $.identifier),

    _dotted_name: ($) => prec.right(seq($.identifier, repeat(seq('.', $.identifier)))),

    columns_expression: ($) =>
      seq(optional('*'), kw('columns'), parens($._expression)),

    function_call: ($) =>
      prec.right(
        seq(
          field('function', $.qualified_name),
          $.function_arguments,
          optional(seq(kw('within'), kw('group'), parens($.order_by_clause))),
          optional($.filter_clause),
          optional(kw('export_state')),
          optional($.over_clause)
        )
      ),

    _function_call_arguments: ($) =>
      choice(
        seq(
          choice(kw('distinct'), kw('all')),
          optional(list($._function_argument)),
          optional($.order_by_clause),
          optional($._nulls_respect)
        ),
        seq(list($._function_argument), optional($.order_by_clause), optional($._nulls_respect)),
        seq($.order_by_clause, optional($._nulls_respect)),
        $._nulls_respect
      ),

    _nulls_respect: ($) => seq(choice(kw('ignore'), kw('respect')), kw('nulls')),

    _function_argument: ($) => choice($.named_argument, $._expression),

    function_arguments: ($) => parens(optional($._function_call_arguments)),

    filter_clause: ($) =>
      seq(kw('filter'), parens(optional(kw('where')), $._expression)),

    column_aliases: ($) => parens(list($.identifier)),

    column_list: ($) => parens(list($.identifier)),

    value_row: ($) => parens(list($._expression)),

    target_list: ($) => prec.right(list($.aliased_expression)),

    named_argument: ($) =>
      seq(field('name', $.identifier), choice(':=', '=>'), $._expression),

    // OVER (w) is covered by window_frame_definition's base-name form.
    over_clause: ($) => seq(kw('over'), choice($.identifier, $.window_frame_definition)),

    window_frame_definition: ($) =>
      parens(
        optional(field('base', $.identifier)),
        optional(seq(kw('partition'), kw('by'), list($._expression))),
        optional($.order_by_clause),
        optional($.frame_clause)
      ),

    frame_clause: ($) =>
      prec.right(
        seq(
          choice(kw('rows'), kw('range'), kw('groups')),
          choice(seq(kw('between'), $._frame_bound, kw('and'), $._frame_bound), $._frame_bound),
          optional(
            seq(
              kw('exclude'),
              choice(
                seq(kw('current'), kw('row')),
                kw('group'),
                kw('ties'),
                seq(kw('no'), kw('others'))
              )
            )
          )
        )
      ),

    _frame_bound: ($) =>
      choice(
        seq(kw('unbounded'), choice(kw('preceding'), kw('following'))),
        seq(kw('current'), kw('row')),
        seq($._expression, choice(kw('preceding'), kw('following')))
      ),

    extract_expression: ($) =>
      seq(
        kw('extract'),
        parens(choice($.identifier, $.string_literal), kw('from'), $._expression)
      ),

    position_expression: ($) =>
      seq(kw('position'), parens($._expression, kw('in'), $._expression)),

    trim_expression: ($) =>
      seq(
        kw('trim'),
        parens(
          optional(choice(kw('both'), kw('leading'), kw('trailing'))),
          optional(seq(optional($._expression), kw('from'))),
          list($._expression)
        )
      ),

    legacy_lambda_expression: ($) =>
      prec.right(seq(kw('lambda'), list($.identifier), ':', $._expression)),

    list_expression: ($) =>
      seq(optional(kw('array')), '[', optional(list($._expression)), ']'),

    list_comprehension: ($) =>
      seq(
        '[',
        $._expression,
        kw('for'),
        list($.identifier),
        kw('in'),
        $._expression,
        optional(seq(kw('if'), $._expression)),
        ']'
      ),

    struct_expression: ($) => seq('{', optional(list($.struct_field)), '}'),
    struct_field: ($) =>
      seq(field('name', choice($.identifier, $.string_literal)), ':', $._expression),

    map_expression: ($) =>
      seq(kw('map'), '{', optional(list(seq($._expression, ':', $._expression))), '}'),

    interval_literal: ($) =>
      prec.right(
        seq(
          kw('interval'),
          choice($.string_literal, $.number_literal, parens($._expression)),
          optional($.identifier)
        )
      ),

    // TypeLiteral <- Type StringLiteral, e.g. DATE '2024-01-01'.
    type_literal: ($) => prec(1, seq($.qualified_name, $.string_literal)),

    // ============================== TYPES =============================
    type: ($) =>
      prec.right(
        seq(
          choice(
            $.struct_type,
            $.map_type,
            $.union_type,
            $.generic_type
          ),
          repeat(choice(seq('[', optional($._expression), ']'), kw('array')))
        )
      ),

    generic_type: ($) =>
      prec.right(
        seq(
          $.qualified_name,
          optional(choice(parens(list($._expression)), seq(kw('precision')))),
          optional(seq(choice(kw('with'), kw('without')), kw('time'), kw('zone'))),
          optional(kw('varying'))
        )
      ),

    struct_type: ($) =>
      seq(choice(kw('struct'), kw('row')), parens(list(seq($.identifier, $.type)))),

    map_type: ($) => seq(kw('map'), parens(list($.type))),

    union_type: ($) => seq(kw('union'), parens(list(seq($.identifier, $.type)))),

    // ============================ STATEMENTS ==========================
    insert_statement: ($) =>
      prec.right(
        seq(
          optional($.with_clause),
          kw('insert'),
          optional(seq(kw('or'), choice(kw('replace'), kw('ignore')))),
          kw('into'),
          field('table', $.qualified_name),
          optional(seq(kw('as'), $.identifier)),
          optional(seq(kw('by'), choice(kw('name'), kw('position')))),
          optional($.column_list),
          choice($.select_statement, seq(kw('default'), kw('values'))),
          optional($.on_conflict_clause),
          optional($.returning_clause)
        )
      ),

    on_conflict_clause: ($) =>
      seq(
        kw('on'),
        kw('conflict'),
        optional(seq(parens(list($.identifier)), optional($.where_clause))),
        choice(
          seq(kw('do'), kw('update'), kw('set'), list($.update_set_element), optional($.where_clause)),
          seq(kw('do'), kw('nothing'))
        )
      ),

    returning_clause: ($) => prec.right(seq(kw('returning'), list($.aliased_expression))),

    update_statement: ($) =>
      prec.right(
        seq(
          optional($.with_clause),
          kw('update'),
          field('table', $.qualified_name),
          optional(seq(optional(kw('as')), $.identifier)),
          kw('set'),
          choice(
            list($.update_set_element),
            seq(parens(list($.identifier)), '=', $._expression)
          ),
          optional($.from_clause),
          optional($.where_clause),
          optional($.returning_clause)
        )
      ),

    update_set_element: ($) => seq($._dotted_name, '=', $._expression),

    delete_statement: ($) =>
      prec.right(
        seq(
          optional($.with_clause),
          kw('delete'),
          kw('from'),
          field('table', $.qualified_name),
          optional(seq(optional(kw('as')), $.identifier)),
          optional(seq(kw('using'), list($.table_ref))),
          optional($.where_clause),
          optional($.returning_clause)
        )
      ),

    truncate_statement: ($) =>
      seq(kw('truncate'), optional(kw('table')), $.qualified_name),

    // create_table.gram, create_view/schema/macro.gram.
    create_statement: ($) =>
      seq(
        kw('create'),
        optional(seq(kw('or'), kw('replace'))),
        optional(choice(kw('temp'), kw('temporary'), kw('persistent'))),
        choice(
          $.create_table,
          $.create_view,
          $.create_schema,
          $.create_macro,
          $.create_sequence,
          $.create_index,
          $.create_type
        )
      ),

    create_table: ($) =>
      prec.right(
        seq(
          kw('table'),
          optional($._if_not_exists),
          field('name', $.qualified_name),
          choice(
            seq(
              optional($.column_list),
              kw('as'),
              $._statement
            ),
            seq(
              parens(optional(list(choice($.column_definition, $.table_constraint))))
            )
          )
        )
      ),

    column_definition: ($) =>
      prec.right(
        seq(
          field('name', $._dotted_name),
          optional(field('type', $.type)),
          optional(seq(optional(seq(kw('generated'), optional(choice(kw('always'), seq(kw('by'), kw('default')))))), kw('as'), parens($._expression), optional(choice(kw('virtual'), kw('stored'))))),
          repeat($.column_constraint)
        )
      ),

    column_constraint: ($) =>
      choice(
        seq(kw('not'), kw('null')),
        kw('null'),
        kw('unique'),
        seq(kw('primary'), kw('key')),
        seq(kw('default'), $._expression),
        seq(kw('check'), parens($._expression)),
        seq(kw('references'), $.qualified_name, optional(parens(list($.identifier)))),
        seq(kw('collate'), $._dotted_name)
      ),

    table_constraint: ($) =>
      seq(
        optional(seq(kw('constraint'), $.identifier)),
        choice(
          seq(kw('primary'), kw('key'), parens(list($.identifier))),
          seq(kw('unique'), parens(list($.identifier))),
          seq(kw('check'), parens($._expression)),
          seq(
            kw('foreign'),
            kw('key'),
            parens(list($.identifier)),
            kw('references'),
            $.qualified_name,
            optional(parens(list($.identifier)))
          )
        )
      ),

    create_view: ($) =>
      seq(
        optional(kw('recursive')),
        kw('view'),
        optional($._if_not_exists),
        field('name', $.qualified_name),
        optional($.column_list),
        kw('as'),
        $.select_statement
      ),

    create_schema: ($) =>
      seq(kw('schema'), optional($._if_not_exists), field('name', $.qualified_name)),

    create_macro: ($) =>
      seq(
        choice(kw('macro'), kw('function')),
        optional($._if_not_exists),
        field('name', $.qualified_name),
        list(
          seq(
            parens(optional(list(choice($.named_argument, seq($.identifier, optional($.type)))))),
            kw('as'),
            choice(seq(kw('table'), $.select_statement), $._expression)
          )
        )
      ),

    create_sequence: ($) =>
      prec.right(
        seq(
          kw('sequence'),
          optional($._if_not_exists),
          field('name', $.qualified_name),
          repeat(choice($.identifier, $.number_literal, kw('with'), kw('no')))
        )
      ),

    create_index: ($) =>
      prec.right(
        seq(
          optional(kw('unique')),
          kw('index'),
          optional($._if_not_exists),
          field('name', $.identifier),
          kw('on'),
          $.qualified_name,
          optional(seq(kw('using'), $.identifier)),
          parens(list($._expression))
        )
      ),

    create_type: ($) =>
      seq(
        kw('type'),
        optional($._if_not_exists),
        field('name', $.qualified_name),
        kw('as'),
        choice(seq(kw('enum'), parens(list($.string_literal))), $.type)
      ),

    _if_not_exists: ($) => seq(kw('if'), kw('not'), kw('exists')),
    _if_exists: ($) => seq(kw('if'), kw('exists')),

    drop_statement: ($) =>
      prec.right(
        seq(
          kw('drop'),
          choice(
            kw('table'),
            kw('view'),
            seq(kw('materialized'), kw('view')),
            kw('macro'),
            kw('function'),
            kw('schema'),
            kw('index'),
            kw('sequence'),
            kw('type'),
            seq(optional(choice(kw('temp'), kw('temporary'), kw('persistent'))), kw('secret'))
          ),
          optional($._if_exists),
          list($.qualified_name),
          optional(choice(kw('cascade'), kw('restrict')))
        )
      ),

    attach_statement: ($) =>
      prec.right(
        seq(
          kw('attach'),
          optional(seq(kw('or'), kw('replace'))),
          optional($._if_not_exists),
          optional(kw('database')),
          $._expression,
          optional(seq(kw('as'), $.identifier)),
          optional(parens(list(seq($._col_label, optional($._expression)))))
        )
      ),

    detach_statement: ($) =>
      seq(kw('detach'), optional(kw('database')), optional($._if_exists), $.identifier),

    use_statement: ($) => seq(kw('use'), $.qualified_name),

    set_statement: ($) =>
      seq(
        kw('set'),
        choice(
          seq(kw('schema'), $.string_literal),
          seq(kw('time'), kw('zone'), $._expression),
          seq(
            optional(choice(kw('local'), kw('session'), kw('global'), kw('variable'))),
            field('setting', $.identifier),
            choice('=', kw('to')),
            list($._expression)
          )
        )
      ),

    reset_statement: ($) =>
      seq(
        kw('reset'),
        optional(choice(kw('local'), kw('session'), kw('global'), kw('variable'))),
        field('setting', $.identifier)
      ),

    pragma_statement: ($) =>
      prec.right(
        seq(
          kw('pragma'),
          field('name', $.identifier),
          optional(choice(seq('=', list($._expression)), parens(list($._expression))))
        )
      ),

    call_statement: ($) =>
      seq(
        kw('call'),
        field('function', $.qualified_name),
        $.function_arguments
      ),

    explain_statement: ($) =>
      prec(
        1,
        seq(
          kw('explain'),
          optional(kw('analyze')),
          optional(parens(list(seq($.identifier, optional($._expression))))),
          $._statement
        )
      ),

    transaction_statement: ($) =>
      prec.right(
        choice(
          seq(
            choice(kw('begin'), kw('start')),
            optional(choice(kw('work'), kw('transaction'))),
            optional(seq(kw('read'), choice(kw('only'), kw('write'))))
          ),
          seq(choice(kw('commit'), kw('end')), optional(choice(kw('work'), kw('transaction')))),
          seq(choice(kw('rollback'), kw('abort')), optional(choice(kw('work'), kw('transaction'))))
        )
      ),

    // copy.gram, simplified: the two common shapes. Full fidelity is a
    // phase-2 item; unrecognized forms error-recover harmlessly.
    copy_statement: ($) =>
      prec.right(
        seq(
          kw('copy'),
          choice(
            seq($.qualified_name, optional($.column_list)),
            parens($._statement)
          ),
          choice(kw('to'), kw('from')),
          choice($.string_literal, $.identifier),
          optional(parens(list(seq($._col_label, optional($._expression)))))
        )
      ),

    load_statement: ($) =>
      prec.right(
        seq(
          kw('load'),
          choice($.identifier, $.string_literal),
          optional(seq(kw('from'), choice($.identifier, $.string_literal))),
          optional(seq(kw('as'), $.identifier))
        )
      ),

    install_statement: ($) =>
      prec.right(
        seq(
          optional(kw('force')),
          kw('install'),
          optional(seq(kw('and'), kw('load'))),
          choice($.identifier, $.string_literal),
          optional(seq(kw('from'), choice($.identifier, $.string_literal))),
          optional(seq(kw('version'), choice($.identifier, $.string_literal)))
        )
      ),

    execute_statement: ($) =>
      prec.right(
        seq(
          kw('execute'),
          $.identifier,
          optional($.function_arguments)
        )
      ),

    prepare_statement: ($) =>
      seq(
        kw('prepare'),
        $.identifier,
        optional(parens(list($.type))),
        kw('as'),
        $._statement
      ),

    deallocate_statement: ($) =>
      seq(kw('deallocate'), optional(kw('prepare')), $.identifier),

    vacuum_statement: ($) =>
      prec.right(
        seq(
          kw('vacuum'),
          optional(parens(list($.identifier))),
          optional(seq($.qualified_name, optional(parens(list($.identifier)))))
        )
      ),

    checkpoint_statement: ($) =>
      prec.right(seq(optional(kw('force')), kw('checkpoint'), optional($.identifier))),

    analyze_statement: ($) =>
      prec.right(
        1,
        seq(
          choice(kw('analyze'), kw('analyse')),
          optional(kw('verbose')),
          optional(seq($.qualified_name, optional(parens(list($.identifier)))))
        )
      ),

    export_statement: ($) =>
      prec.right(
        seq(
          kw('export'),
          kw('database'),
          optional(seq($.identifier, kw('to'))),
          $.string_literal,
          optional(parens(list(seq($._col_label, optional($._expression)))))
        )
      ),

    import_statement: ($) => seq(kw('import'), kw('database'), $.string_literal),

    // common.gram: ExpressionStatement — bare `1 + 1` is a statement.
    expression_statement: ($) => prec(-1, list($.aliased_expression)),

    // ============================== NAMES =============================
    qualified_name: ($) =>
      prec.right(seq($.identifier, repeat(seq('.', $.identifier)))),

    // create_table.gram ColLabel: any keyword class or identifier — after
    // a dot, everything is a name. Reserved keywords appear here via the
    // lexer's contextual keyword handling.
    _col_label: ($) => $.identifier,

    identifier: ($) => choice($.plain_identifier, $.quoted_identifier),

    plain_identifier: ($) => /[a-zA-Z_\x80-\xff][a-zA-Z0-9_\x80-\xff]*/,

    // The matcher's semantics, not the .gram approximation: "" escapes.
    quoted_identifier: ($) => /"([^"]|"")*"/,

    // Matcher semantics: '' escaping, E'...' escapes, $$...$$ (untagged).
    string_literal: ($) =>
      token(
        choice(
          seq(/[eE]/, "'", repeat(choice(/[^'\\]/, /\\./, "''")), "'"),
          seq("'", repeat(choice(/[^']/, "''")), "'"),
          seq('$$', /[^$]*(\$[^$]+)*/, '$$')
        )
      ),

    number_literal: ($) =>
      token(
        choice(
          /0[xX][0-9a-fA-F][0-9a-fA-F_]*/,
          /0[bB][01][01_]*/,
          /[0-9][0-9_]*(\.[0-9_]*)?([eE][+-]?[0-9]+)?/,
          /\.[0-9][0-9_]*([eE][+-]?[0-9]+)?/
        )
      ),

    comment: ($) => token(seq('--', /[^\n]*/)),

    block_comment: ($) => token(seq('/*', /[^*]*\*+([^/*][^*]*\*+)*/, '/')),
  },
});
