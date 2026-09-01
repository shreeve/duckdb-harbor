; tree-sitter-duckdb highlights, targeting gpui-component's recognized
; capture names. Keywords are the grammar's anonymous aliased tokens —
; this block is regenerated from grammar.js by grammar-sync.

(comment) @comment
(block_comment) @comment

(string_literal) @string
(number_literal) @number

["TRUE" "FALSE"] @boolean
"NULL" @constant

(parameter) @variable.special
(positional_reference) @variable.special

; the last identifier of a call target is the function name
(function_call
  function: (qualified_name (identifier) @function .))
(table_function_call
  function: (qualified_name (identifier) @function .))
(method_call
  method: (identifier) @function)
(call_statement
  function: (qualified_name (identifier) @function .))

; tables and types
(base_table_ref
  name: (qualified_name (identifier) @type .))
(generic_type
  (qualified_name (identifier) @type .))
(struct_type) @type
(map_type) @type
(union_type) @type

; members and names
(field_access field: (identifier) @property)
(struct_field name: (identifier) @property)
(named_argument name: (identifier) @property)
(aliased_expression alias: (identifier) @variable)
(table_alias alias: (identifier) @variable)
(table_alias_colon alias: (identifier) @variable)
(column_definition name: (identifier) @property)

["(" ")" "[" "]" "{" "}"] @punctuation.bracket
["," ";" "." ":"] @punctuation.delimiter

[
  "->" "->>" ":=" "=>" "::"
  "+" "-" "*" "/" "//" "%" "^" "**"
  "=" "==" "!=" "<>" "<" ">" "<=" ">="
  "||" "^@" "&&" "@>" "<@" "&" "|" "<<" ">>"
  "~" "~*" "~~" "~~*" "~~~" "!~" "!~*" "!~~" "!~~*"
  ">>=" "<<=" "!"
] @operator

"USING SAMPLE" @keyword

[
  "ABORT"
  "ALL"
  "ALWAYS"
  "ANALYSE"
  "ANALYZE"
  "AND"
  "ANTI"
  "ARRAY"
  "AS"
  "ASC"
  "ASCENDING"
  "ASOF"
  "AT"
  "ATTACH"
  "BEGIN"
  "BETWEEN"
  "BOTH"
  "BY"
  "CALL"
  "CASCADE"
  "CASE"
  "CAST"
  "CHECK"
  "CHECKPOINT"
  "COLLATE"
  "COLUMNS"
  "COMMIT"
  "CONFLICT"
  "CONSTRAINT"
  "COPY"
  "CREATE"
  "CROSS"
  "CUBE"
  "CURRENT"
  "DATABASE"
  "DEALLOCATE"
  "DEFAULT"
  "DELETE"
  "DESC"
  "DESCENDING"
  "DESCRIBE"
  "DETACH"
  "DISTINCT"
  "DO"
  "DROP"
  "ELSE"
  "END"
  "ENUM"
  "ESCAPE"
  "EXCEPT"
  "EXCLUDE"
  "EXECUTE"
  "EXISTS"
  "EXPLAIN"
  "EXPORT"
  "EXPORT_STATE"
  "EXTRACT"
  "FALSE"
  "FILTER"
  "FIRST"
  "FOLLOWING"
  "FOR"
  "FORCE"
  "FOREIGN"
  "FROM"
  "FULL"
  "FUNCTION"
  "GENERATED"
  "GLOB"
  "GLOBAL"
  "GROUP"
  "GROUPING"
  "GROUPS"
  "HAVING"
  "IF"
  "IGNORE"
  "ILIKE"
  "IMPORT"
  "IN"
  "INDEX"
  "INNER"
  "INSERT"
  "INSTALL"
  "INTERSECT"
  "INTERVAL"
  "INTO"
  "IS"
  "ISNULL"
  "JOIN"
  "KEY"
  "LAMBDA"
  "LAST"
  "LATERAL"
  "LEADING"
  "LEFT"
  "LIKE"
  "LIMIT"
  "LOAD"
  "LOCAL"
  "MACRO"
  "MAP"
  "MATERIALIZED"
  "NAME"
  "NATURAL"
  "NO"
  "NOT"
  "NOTHING"
  "NOTNULL"
  "NULL"
  "NULLS"
  "OFFSET"
  "ON"
  "ONLY"
  "OR"
  "ORDER"
  "ORDINALITY"
  "OTHERS"
  "OUTER"
  "OVER"
  "PARTITION"
  "PERCENT"
  "PERSISTENT"
  "POSITION"
  "POSITIONAL"
  "PRAGMA"
  "PRECEDING"
  "PRECISION"
  "PREPARE"
  "PRIMARY"
  "QUALIFY"
  "RANGE"
  "READ"
  "RECURSIVE"
  "REFERENCES"
  "RENAME"
  "REPEATABLE"
  "REPLACE"
  "RESET"
  "RESPECT"
  "RESTRICT"
  "RETURNING"
  "RIGHT"
  "ROLLBACK"
  "ROLLUP"
  "ROW"
  "ROWS"
  "SCHEMA"
  "SECRET"
  "SELECT"
  "SEMI"
  "SEQUENCE"
  "SESSION"
  "SET"
  "SETS"
  "SHOW"
  "SIMILAR"
  "START"
  "STORED"
  "STRUCT"
  "SUMMARIZE"
  "TABLE"
  "TABLES"
  "TABLESAMPLE"
  "TEMP"
  "TEMPORARY"
  "THEN"
  "TIES"
  "TIME"
  "TIMESTAMP"
  "TO"
  "TRAILING"
  "TRANSACTION"
  "TRIM"
  "TRUE"
  "TRUNCATE"
  "TRY_CAST"
  "TYPE"
  "UNBOUNDED"
  "UNION"
  "UNIQUE"
  "UNKNOWN"
  "UPDATE"
  "USE"
  "USING"
  "VACUUM"
  "VALUES"
  "VARIABLE"
  "VARYING"
  "VERBOSE"
  "VERSION"
  "VIEW"
  "VIRTUAL"
  "WHEN"
  "WHERE"
  "WINDOW"
  "WITH"
  "WITHIN"
  "WITHOUT"
  "WORK"
  "WRITE"
  "ZONE"
] @keyword
