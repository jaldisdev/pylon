use super::{f, p, E};
use super::{Float64, FnDescriptor, Vector};

pub(super) fn build() -> Vec<FnDescriptor> {
    vec![
        f("pgvector", "euclidean_distance",  vec![p("a", Vector), p("b", Vector)], Float64, E("($1 <-> $2)")),
        f("pgvector", "cosine_distance",     vec![p("a", Vector), p("b", Vector)], Float64, E("($1 <=> $2)")),
        f("pgvector", "neg_inner_product",   vec![p("a", Vector), p("b", Vector)], Float64, E("($1 <#> $2)")),
        f("pgvector", "inner_product",       vec![p("a", Vector), p("b", Vector)], Float64, E("(0.0 - ($1 <#> $2))")),
    ]
}
