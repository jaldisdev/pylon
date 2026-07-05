use pylon_core::query::compile;
use pylon_core::schema::{
    LinkDescriptor, MultiLinkDescriptor, PropertyDescriptor, RewriteEntry, SchemaDescriptor,
    TypeDescriptor,
};

fn main() {
    let schema = SchemaDescriptor {
        types: vec![
            TypeDescriptor {
                name: "Person".into(),
                module: "default".into(),
                table: "Person".into(),
                abstract_: false,
                materialized: false,
                description: None,
                parents: vec![],
                interfaces: vec![],
                properties: vec![
                    PropertyDescriptor {
                        name: "id".into(),
                        pg_type: "uuid".into(),
                        nullable: false,
                        default_sql: Some("uuidv7()".into()),
                        default_pyql: None,
                        description: None,
                        check_constraints: vec![],
                        is_exclusive: true,
                        is_pk: true,
                        is_readonly: true,
                        rewrites: vec![],
                    },
                    PropertyDescriptor {
                        name: "name".into(),
                        pg_type: "text".into(),
                        nullable: false,
                        default_sql: None,
                        default_pyql: None,
                        description: None,
                        check_constraints: vec![],
                        is_exclusive: false,
                        is_pk: false,
                        is_readonly: false,
                        rewrites: vec![],
                    },
                    PropertyDescriptor {
                        name: "age".into(),
                        pg_type: "int8".into(),
                        nullable: true,
                        default_sql: None,
                        default_pyql: None,
                        description: None,
                        check_constraints: vec![],
                        is_exclusive: false,
                        is_pk: false,
                        is_readonly: false,
                        rewrites: vec![],
                    },
                    PropertyDescriptor {
                        name: "slug".into(),
                        pg_type: "text".into(),
                        nullable: true,
                        default_sql: None,
                        default_pyql: None,
                        description: None,
                        check_constraints: vec![],
                        is_exclusive: false,
                        is_pk: false,
                        is_readonly: false,
                        rewrites: vec![
                            RewriteEntry { on: 1, handler: "str_lower(.name)".into() },
                            RewriteEntry { on: 2, handler: "str_lower(.name)".into() },
                        ],
                    },
                ],
                links: vec![LinkDescriptor {
                    name: "company".into(),
                    target: "default::Company".into(),
                    nullable: true,
                    description: None,
                    default_pyql: None,
                    is_exclusive: false,
                    is_readonly: false,
                    rewrites: vec![],
                    on_delete: vec![],
                }],
                multilinks: vec![MultiLinkDescriptor {
                    name: "posts".into(),
                    target: "default::Post".into(),
                    through: None,
                    nullable: false,
                    description: None,
                    default_pyql: None,
                    on_delete: vec![],
                }],
                computed: vec![],
                constraints: vec![],
                indexes: vec![],
                vector_indexes: vec![],
                search_indexes: vec![],
                triggers: vec![],
                junction: false,
            },
            TypeDescriptor {
                name: "Company".into(),
                module: "default".into(),
                table: "Company".into(),
                abstract_: false,
                materialized: false,
                description: None,
                parents: vec![],
                interfaces: vec![],
                properties: vec![PropertyDescriptor {
                    name: "name".into(),
                    pg_type: "text".into(),
                    nullable: false,
                    default_sql: None,
                        default_pyql: None,
                    description: None,
                    check_constraints: vec![],
                    is_exclusive: false,
                    is_pk: false,
                    is_readonly: false,
                    rewrites: vec![],
                }],
                links: vec![],
                multilinks: vec![],
                computed: vec![],
                constraints: vec![],
                indexes: vec![],
                vector_indexes: vec![],
                search_indexes: vec![],
                triggers: vec![],
                junction: false,
            },
            TypeDescriptor {
                name: "Post".into(),
                module: "default".into(),
                table: "Post".into(),
                abstract_: false,
                materialized: false,
                description: None,
                parents: vec![],
                interfaces: vec![],
                properties: vec![
                    PropertyDescriptor {
                        name: "title".into(),
                        pg_type: "text".into(),
                        nullable: false,
                        default_sql: None,
                        default_pyql: None,
                        description: None,
                        check_constraints: vec![],
                        is_exclusive: false,
                        is_pk: false,
                        is_readonly: false,
                        rewrites: vec![],
                    },
                    PropertyDescriptor {
                        name: "body".into(),
                        pg_type: "text".into(),
                        nullable: true,
                        default_sql: None,
                        default_pyql: None,
                        description: None,
                        check_constraints: vec![],
                        is_exclusive: false,
                        is_pk: false,
                        is_readonly: false,
                        rewrites: vec![],
                    },
                ],
                links: vec![],
                multilinks: vec![],
                computed: vec![],
                constraints: vec![],
                indexes: vec![],
                vector_indexes: vec![],
                search_indexes: vec![],
                triggers: vec![],
                junction: false,
            },
        ],
        scalars: vec![],
        enums: vec![],
        globals: vec![],
        functions: vec![],
        aliases: vec![],
    };

    let queries = [
        (
            "SELECT with shape + filter + limit",
            "SELECT Person { name, age } FILTER .age > $min_age ORDER BY .name ASC LIMIT 10",
        ),
        (
            "SELECT with single link",
            "SELECT Person { name, company { name } }",
        ),
        (
            "SELECT with multi-link",
            "SELECT Person { name, posts { title, body } }",
        ),
        (
            "INSERT bare (returns id only)",
            "INSERT Person { name := $name, age := $age }",
        ),
        (
            "SELECT over INSERT (explicit shape)",
            "SELECT (INSERT Person { name := $name, age := $age }) { id, name }",
        ),
        (
            "UPDATE bare (returns id only)",
            "UPDATE Person FILTER .id = $id SET { name := $name, age := $age }",
        ),
        (
            "SELECT over UPDATE (explicit shape)",
            "SELECT (UPDATE Person FILTER .id = $id SET { name := $name }) { id, name }",
        ),
        (
            "DELETE bare (returns id only)",
            "DELETE Person FILTER .name = $name",
        ),
        (
            "SELECT over DELETE (explicit shape)",
            "SELECT (DELETE Person FILTER .id = $id) { id, name }",
        ),
        (
            "INSERT with link assignment (subquery)",
            "INSERT Person { name := $name, company := (SELECT Company FILTER .name = $co) }",
        ),
        (
            "UPDATE with link assignment (subquery)",
            "UPDATE Person FILTER .id = $id SET { name := $name, company := (SELECT Company FILTER .name = $co) }",
        ),
        (
            "SELECT over SELECT (inner filter, outer shape)",
            "SELECT (SELECT Person FILTER .age > 18) { name }",
        ),
        (
            "INSERT upsert (UNLESS CONFLICT DO NOTHING)",
            "INSERT Person { name := $name } UNLESS CONFLICT ON .name",
        ),
        (
            "INSERT upsert (UNLESS CONFLICT DO UPDATE)",
            "INSERT Person { name := $name, age := $age } UNLESS CONFLICT ON .name ELSE (UPDATE Person SET { age := $age })",
        ),
    ];

    for (label, query) in &queries {
        println!("━━━ {} ━━━", label);
        println!("PyQL:  {}", query);
        match compile(query, &schema) {
            Ok(compiled) => {
                println!("SQL:\n{}", compiled.sql);
            }
            Err(e) => println!("ERROR: {:?}", e),
        }
        println!();
    }
}
