# `math`

| Function | Signature(s) | Returns | Description |
|---|---|---|---|
| `pi` | `()` | `float64` | The constant π. |
| `e` | `()` | `float64` | Euler's number. |
| `exp` | `(n: float64)` | `float64` | e^n. |
| `ln` | `(n: float64)` | `float64` | Natural logarithm. |
| `log` | `(n: float64 [, base: float64])` | `float64` | Base-10 logarithm (1-arg), or logarithm to an arbitrary `base` (2-arg). |
| `log2` | `(n: float64)` | `float64` | Base-2 logarithm. |
| `log10` | `(n: float64)` | `float64` | Base-10 logarithm — the preferred spelling; `lg` (below) is a thin alias kept for familiarity. |
| `lg` | `(n: int64\|float64\|decimal)` | matching float/decimal type | Base-10 logarithm — alias for `log10`, but overloaded across more input types. |
| `sin` / `cos` / `tan` / `cot` | `(n: float64)` | `float64` | Trigonometric functions (radians). |
| `asin` / `acos` / `atan` | `(n: float64)` | `float64` | Inverse trigonometric functions. |
| `atan2` | `(y: float64, x: float64)` | `float64` | Two-argument arctangent. |
| `stddev` / `stddev_pop` | `(set of float64\|decimal)` | matching type | Sample / population standard deviation. |
| `var` / `var_pop` | `(set of float64\|decimal)` | matching type | Sample / population variance. |

Note that most other numeric functions (`abs`, `ceil`, `floor`, `round`, `sign`, `sqrt`) live in [`std`](std.md#numeric), not `math` — a deliberate Pylon choice, reserving `math` for the more specialized trigonometric/statistical functions.
