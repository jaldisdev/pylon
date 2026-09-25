//
// This source file is part of the Pylon open source project.
//
// Copyright (c) 2026 Jaldis B.V.
//
// Licensed under the MIT OR Apache-2.0 license (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://opensource.org/licenses/MIT
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//

use super::{B, E, f, p, set_of};
use super::{Decimal, Float64, FnDescriptor, Int64};

pub(super) fn build() -> Vec<FnDescriptor> {
    vec![
        f("math", "pi", vec![], Float64, E("pi()")),
        f("math", "e", vec![], Float64, E("exp(1.0)")),
        f("math", "exp", vec![p("n", Float64)], Float64, B("exp")),
        f("math", "ln", vec![p("n", Float64)], Float64, B("ln")),
        f("math", "log", vec![p("n", Float64)], Float64, B("log")),
        // Two-arg form: PyQL log(n, base) → PG log(base, n) — arguments are swapped.
        f(
            "math",
            "log",
            vec![p("n", Float64), p("base", Float64)],
            Float64,
            E("log($2, $1)"),
        ),
        f("math", "log2", vec![p("n", Float64)], Float64, E("log(2.0, $1)")),
        f("math", "log10", vec![p("n", Float64)], Float64, B("log")),
        // math::lg is exactly a base-10 log; kept as a thin alias alongside
        // the clearer std::log10 name (which Pylon deliberately favors — see
        // stdlib-intentional-divergences memory).
        f("math", "lg", vec![p("n", Int64)], Float64, E("log($1::float8)")),
        f("math", "lg", vec![p("n", Float64)], Float64, B("log")),
        f("math", "lg", vec![p("n", Decimal)], Decimal, B("log")),
        f("math", "sin", vec![p("n", Float64)], Float64, B("sin")),
        f("math", "cos", vec![p("n", Float64)], Float64, B("cos")),
        f("math", "tan", vec![p("n", Float64)], Float64, B("tan")),
        f("math", "cot", vec![p("n", Float64)], Float64, B("cot")),
        f("math", "asin", vec![p("n", Float64)], Float64, B("asin")),
        f("math", "acos", vec![p("n", Float64)], Float64, B("acos")),
        f("math", "atan", vec![p("n", Float64)], Float64, B("atan")),
        f(
            "math",
            "atan2",
            vec![p("y", Float64), p("x", Float64)],
            Float64,
            B("atan2"),
        ),
        f(
            "math",
            "stddev",
            vec![p("s", set_of(Float64))],
            Float64,
            B("stddev_samp"),
        ),
        f(
            "math",
            "stddev",
            vec![p("s", set_of(Decimal))],
            Decimal,
            B("stddev_samp"),
        ),
        f(
            "math",
            "stddev_pop",
            vec![p("s", set_of(Float64))],
            Float64,
            B("stddev_pop"),
        ),
        f(
            "math",
            "stddev_pop",
            vec![p("s", set_of(Decimal))],
            Decimal,
            B("stddev_pop"),
        ),
        f("math", "var", vec![p("s", set_of(Float64))], Float64, B("var_samp")),
        f("math", "var", vec![p("s", set_of(Decimal))], Decimal, B("var_samp")),
        f("math", "var_pop", vec![p("s", set_of(Float64))], Float64, B("var_pop")),
        f("math", "var_pop", vec![p("s", set_of(Decimal))], Decimal, B("var_pop")),
        // These belong in `math::`, not `std::` — `std::ceil` alone would
        // leave `math::ceil(…)` reading as a function that does not exist.
        f("math", "abs", vec![p("x", Int64)], Int64, B("abs")),
        f("math", "abs", vec![p("x", Float64)], Float64, B("abs")),
        f("math", "abs", vec![p("x", Decimal)], Decimal, B("abs")),
        f("math", "ceil", vec![p("x", Int64)], Int64, B("ceil")),
        f("math", "ceil", vec![p("x", Float64)], Float64, B("ceil")),
        f("math", "ceil", vec![p("x", Decimal)], Decimal, B("ceil")),
        f("math", "floor", vec![p("x", Int64)], Int64, B("floor")),
        f("math", "floor", vec![p("x", Float64)], Float64, B("floor")),
        f("math", "floor", vec![p("x", Decimal)], Decimal, B("floor")),
        f("math", "sqrt", vec![p("x", Int64)], Float64, E("sqrt($1::float8)")),
        f("math", "sqrt", vec![p("x", Float64)], Float64, B("sqrt")),
        f("math", "sqrt", vec![p("x", Decimal)], Decimal, B("sqrt")),
        f(
            "math",
            "mean",
            vec![p("vals", set_of(Int64))],
            Float64,
            E("avg($1)::float8"),
        ),
        f("math", "mean", vec![p("vals", set_of(Float64))], Float64, B("avg")),
        f("math", "mean", vec![p("vals", set_of(Decimal))], Decimal, B("avg")),
    ]
}
