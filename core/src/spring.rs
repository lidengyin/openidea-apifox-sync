//! Spring MVC mapping: `JavaClass` -> `Endpoint` + JSON Schema.
//!
//! Only the annotations that change the HTTP contract are interpreted; the
//! scanner in `java` already turned the source text into classes/methods.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::java::{ann_attr, ann_first_string, ann_string, split_top_level, Ann, JavaClass, JavaField, JavaFile};
use crate::model::{ApiDoc, Endpoint, Field, Param, ParamIn, Response, Ty};

const WRAPPERS: &[&str] = &[
    "Result",
    "R",
    "ApiResponse",
    "AjaxResult",
    "CommonResult",
    "ResponseResult",
    "ResponseEntity",
    "HttpEntity",
    "Response",
    "Res",
];

const PAGE_TYPES: &[&str] = &["Page", "IPage", "PageResult", "PageInfo", "PagedResult"];

const PRIMITIVE_REQUIRED: &[&str] = &["int", "long", "short", "byte", "double", "float", "boolean", "char"];

const SKIPPED_PARAMS: &[&str] = &[
    "HttpServletRequest",
    "HttpServletResponse",
    "ServletRequest",
    "ServletResponse",
    "HttpSession",
    "Model",
    "ModelMap",
    "BindingResult",
    "Errors",
    "Principal",
    "Locale",
    "InputStream",
    "OutputStream",
    "Reader",
    "Writer",
];

/// Every class of the project, by simple name: cross-module DTOs resolve.
pub struct TypeIndex<'a> {
    /// Canonical key (`Name` when unique, `pkg.Name` for duplicates).
    by_key: HashMap<String, &'a JavaClass>,
    by_simple: HashMap<String, Vec<&'a JavaClass>>,
}

impl<'a> TypeIndex<'a> {
    pub fn build(files: &'a [JavaFile]) -> Self {
        let refs: Vec<&JavaFile> = files.iter().collect();
        Self::build_refs(&refs)
    }

    pub fn build_refs(files: &[&'a JavaFile]) -> Self {
        let mut by_simple: HashMap<String, Vec<&JavaClass>> = HashMap::new();
        for file in files {
            for cls in &file.classes {
                by_simple.entry(cls.name.clone()).or_default().push(cls);
            }
        }
        let mut by_key: HashMap<String, &JavaClass> = HashMap::new();
        for (name, list) in &by_simple {
            for cls in list {
                let key = if list.len() == 1 {
                    name.clone()
                } else {
                    format!("{}.{}", cls.package, cls.name)
                };
                by_key.insert(key, *cls);
            }
        }
        Self { by_key, by_simple }
    }

    /// Resolves a simple type name from `ctx`'s imports / package first.
    fn resolve(&self, ctx: &JavaClass, simple: &str) -> Option<&'a JavaClass> {
        let list = self.by_simple.get(simple)?;
        if list.len() == 1 {
            return Some(list[0]);
        }
        for import in &ctx.imports {
            if import.ends_with(&format!(".{simple}")) {
                if let Some(cls) = self.by_key.get(import) {
                    return Some(*cls);
                }
            }
        }
        let same_package = format!("{}.{}", ctx.package, simple);
        if let Some(cls) = self.by_key.get(&same_package) {
            return Some(*cls);
        }
        list.first().copied()
    }

    /// The `$ref` key for a simple name, disambiguated by package when needed.
    fn key_for(&self, ctx: &JavaClass, simple: &str) -> Option<String> {
        let cls = self.resolve(ctx, simple)?;
        let unique = self.by_simple.get(&cls.name).map(|v| v.len() == 1).unwrap_or(true);
        Some(if unique {
            cls.name.clone()
        } else {
            format!("{}.{}", cls.package, cls.name)
        })
    }

    fn get(&self, key: &str) -> Option<&'a JavaClass> {
        self.by_key.get(key).copied()
    }
}

pub fn scan(files: &[JavaFile]) -> ApiDoc {
    let index = TypeIndex::build(files);
    let refs: Vec<&JavaFile> = files.iter().collect();
    scan_indexed(&index, &refs)
}

/// Endpoints come from `endpoint_files`; types may come from the whole project.
pub fn scan_indexed(index: &TypeIndex, endpoint_files: &[&JavaFile]) -> ApiDoc {
    let mut endpoints = Vec::new();
    for file in endpoint_files {
        for cls in &file.classes {
            if cls.doc.tags.contains_key("ignore") || !is_controller(cls) {
                continue;
            }
            let base = class_base_path(cls);
            for m in &cls.methods {
                if m.doc.tags.contains_key("ignore") {
                    continue;
                }
                let mappings = method_mappings(&m.anns);
                if mappings.is_empty() {
                    continue;
                }
                for (method, sub) in mappings {
                    let path = join_path(&base, &sub);
                    endpoints.extend(build_endpoint(cls, m, &method, &path, index));
                }
            }
        }
    }
    endpoints.sort_by(|a, b| a.path.cmp(&b.path).then(a.method.cmp(&b.method)));

    let mut doc = ApiDoc::new("API 文档");
    doc.endpoints = endpoints;
    doc.schemas = collect_schemas(&doc.endpoints, index);
    doc
}

fn is_controller(cls: &JavaClass) -> bool {
    cls.anns.iter().any(|a| {
        matches!(
            a.name.as_str(),
            "RestController" | "Controller" | "RequestMapping" | "Path" | "FeignClient"
        )
    })
}

fn class_base_path(cls: &JavaClass) -> String {
    for a in &cls.anns {
        if a.name == "FeignClient" {
            return ann_string(&a.args, "path").unwrap_or_default();
        }
        if a.name == "RequestMapping" || a.name == "Path" {
            if let Some(p) = ann_first_string(a) {
                return p;
            }
        }
    }
    String::new()
}

/// `(HTTP method, sub path)` pairs declared by a handler's annotations.
fn method_mappings(anns: &[Ann]) -> Vec<(String, String)> {
    // JAX-RS keeps the path in a separate `@Path`, Feign/Spring in the verb.
    let jaxrs_path = anns
        .iter()
        .find(|a| a.name == "Path")
        .and_then(ann_first_string)
        .unwrap_or_default();
    let mut out = Vec::new();
    for a in anns {
        let spring_verb = match a.name.as_str() {
            "GetMapping" => Some("GET"),
            "PostMapping" => Some("POST"),
            "PutMapping" => Some("PUT"),
            "DeleteMapping" => Some("DELETE"),
            "PatchMapping" => Some("PATCH"),
            _ => None,
        };
        if let Some(v) = spring_verb {
            out.push((v.to_string(), ann_first_string(a).unwrap_or_default()));
            continue;
        }
        let jaxrs_verb = match a.name.as_str() {
            "GET" => Some("GET"),
            "POST" => Some("POST"),
            "PUT" => Some("PUT"),
            "DELETE" => Some("DELETE"),
            "PATCH" => Some("PATCH"),
            "HEAD" => Some("HEAD"),
            "OPTIONS" => Some("OPTIONS"),
            _ => None,
        };
        if let Some(v) = jaxrs_verb {
            out.push((v.to_string(), jaxrs_path.clone()));
            continue;
        }
        if a.name == "RequestMapping" {
            let path = ann_first_string(a).unwrap_or_default();
            let verbs = explicit_verbs(a);
            if verbs.is_empty() {
                out.push(("GET".to_string(), path));
            } else {
                for v in verbs {
                    out.push((v, path.clone()));
                }
            }
        }
    }
    out
}

fn explicit_verbs(ann: &Ann) -> Vec<String> {
    const VERBS: &[&str] = &["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS", "TRACE"];
    let Some(raw) = ann_attr(&ann.args, "method") else {
        return Vec::new();
    };
    let inner = raw.trim().trim_start_matches('{').trim_end_matches('}');
    let mut out = Vec::new();
    for part in split_top_level(inner, ',') {
        let verb = part.trim().rsplit('.').next().unwrap_or("").trim().to_uppercase();
        if VERBS.contains(&verb.as_str()) && !out.contains(&verb) {
            out.push(verb);
        }
    }
    out
}

fn build_endpoint(
    cls: &JavaClass,
    m: &crate::java::JavaMethod,
    method: &str,
    path: &str,
    index: &TypeIndex,
) -> Vec<Endpoint> {
    let mut params = Vec::new();
    let mut body: Option<(String, Ty)> = None;

    for p in &m.params {
        let base = base_type(&p.ty);
        if SKIPPED_PARAMS.contains(&base.as_str()) {
            continue;
        }
        if let Some(a) = p.anns.iter().find(|a| a.name == "RequestBody") {
            let _ = a;
            body = Some(("application/json".into(), parse_ty(&p.ty, index, cls, true)));
            continue;
        }
        if p.anns.iter().any(|a| a.name == "RequestPart") {
            let mut fields = Vec::new();
            fields.push(Field::new(p.name.clone(), Ty::Binary, true, ""));
            body = Some(("multipart/form-data".into(), Ty::Object(fields)));
            continue;
        }
        if base == "MultipartFile" || base == "Part" {
            let mut fields = Vec::new();
            fields.push(Field::new(p.name.clone(), Ty::Binary, true, ""));
            body = Some(("multipart/form-data".into(), Ty::Object(fields)));
            continue;
        }

        let (location, ann, name) = if let Some(a) = p.anns.iter().find(|a| a.name == "PathVariable") {
            (ParamIn::Path, Some(a), ann_name(a).unwrap_or_else(|| p.name.clone()))
        } else if let Some(a) = p.anns.iter().find(|a| a.name == "PathParam") {
            (ParamIn::Path, Some(a), ann_name(a).unwrap_or_else(|| p.name.clone()))
        } else if let Some(a) = p.anns.iter().find(|a| a.name == "RequestParam") {
            (ParamIn::Query, Some(a), ann_name(a).unwrap_or_else(|| p.name.clone()))
        } else if let Some(a) = p.anns.iter().find(|a| a.name == "QueryParam") {
            (ParamIn::Query, Some(a), ann_name(a).unwrap_or_else(|| p.name.clone()))
        } else if let Some(a) = p.anns.iter().find(|a| a.name == "RequestHeader") {
            (ParamIn::Header, Some(a), ann_name(a).unwrap_or_else(|| p.name.clone()))
        } else if let Some(a) = p.anns.iter().find(|a| a.name == "HeaderParam") {
            (ParamIn::Header, Some(a), ann_name(a).unwrap_or_else(|| p.name.clone()))
        } else if let Some(a) = p.anns.iter().find(|a| a.name == "CookieValue") {
            (ParamIn::Cookie, Some(a), ann_name(a).unwrap_or_else(|| p.name.clone()))
        } else if let Some(a) = p.anns.iter().find(|a| a.name == "CookieParam") {
            (ParamIn::Cookie, Some(a), ann_name(a).unwrap_or_else(|| p.name.clone()))
        } else {
            let simple = is_simple(&p.ty, index, cls);
            let read_method = matches!(method, "GET" | "DELETE" | "HEAD" | "OPTIONS");
            if simple || read_method {
                (ParamIn::Query, None, p.name.clone())
            } else {
                body = Some(("application/json".into(), parse_ty(&p.ty, index, cls, true)));
                continue;
            }
        };

        let swagger = p
            .anns
            .iter()
            .find(|a| matches!(a.name.as_str(), "Parameter" | "ApiParam"));
        if let Some(s) = swagger {
            if s.args.contains("hidden = true") || s.args.contains("hidden=true") {
                continue;
            }
        }
        let default = ann
            .and_then(|a| ann_string(&a.args, "defaultValue"))
            .or_else(|| {
                p.anns
                    .iter()
                    .find(|a| a.name == "DefaultValue")
                    .and_then(ann_first_string)
            })
            .filter(|v| !v.is_empty());
        let required = if location == ParamIn::Path {
            true
        } else if default.is_some() {
            false
        } else if let Some(s) = swagger {
            attr_bool(&s.args, "required").unwrap_or(true)
        } else {
            ann.and_then(|a| attr_bool(&a.args, "required")).unwrap_or(true)
        };
        let description = swagger
            .and_then(|s| ann_string(&s.args, "description").or_else(|| ann_string(&s.args, "value")))
            .or_else(|| m.doc.params.get(&name).cloned())
            .unwrap_or_default();
        params.push(Param {
            name,
            location,
            required,
            description,
            ty: parse_ty(&p.ty, index, cls, false),
            default,
        });
    }

    if body.is_none() && !matches!(method, "GET" | "DELETE" | "HEAD" | "OPTIONS") {
        // No @RequestBody: Spring binds a single complex parameter from the body.
        if let Some(p) = m.params.iter().find(|p| {
            is_complex(&p.ty, index, cls)
                && !SKIPPED_PARAMS.contains(&base_type(&p.ty).as_str())
                && p.anns.iter().all(|a| {
                    !matches!(
                        a.name.as_str(),
                        "RequestParam" | "PathVariable" | "RequestHeader" | "RequestPart"
                    )
                })
        }) {
            body = Some(("application/json".into(), parse_ty(&p.ty, index, cls, true)));
        }
    }

    let return_type = if base_type(&m.ret) == "void" {
        None
    } else {
        Some(parse_ty(&m.ret, index, cls, true))
    };
    let media = response_media(&m.anns, &cls.anns, return_type.as_ref());
    let mut declared = api_response_entries(&m.anns);
    if !declared.iter().any(|(code, _)| code.starts_with('2')) {
        declared.insert(0, ("200".to_string(), String::new()));
    }
    let responses = declared
        .into_iter()
        .map(|(code, description)| {
            if code.starts_with('2') {
                Response {
                    description: if description.is_empty() {
                        m.doc.ret.clone()
                    } else {
                        description
                    },
                    media: media.clone(),
                    ty: return_type.clone(),
                    code,
                }
            } else {
                // An error entry without a declared body: description only.
                Response {
                    code,
                    description: "No Content".into(),
                    media: media.clone(),
                    ty: None,
                }
            }
        })
        .collect();

    let summary = operation_summary(&m.anns)
        .filter(|s| !s.is_empty())
        .or_else(|| (!m.doc.summary.is_empty()).then(|| m.doc.summary.clone()))
        .unwrap_or_else(|| m.name.clone());
    let mut description = operation_description(&m.anns)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| m.doc.description.clone());
    if !m.doc.ret.is_empty() {
        if !description.is_empty() {
            description.push_str("\n\n");
        }
        description.push_str("返回：");
        description.push_str(&m.doc.ret);
    }

    let operation_id = format!("{}_{}", lower_first(&cls.name), m.name);
    vec![Endpoint {
        method: method.to_string(),
        path: path.to_string(),
        tag: m
            .doc
            .tags
            .get("module")
            .or_else(|| cls.doc.tags.get("module"))
            .cloned()
            .or_else(|| class_tag_name(cls))
            .unwrap_or_else(|| cls.name.clone()),
        summary,
        description,
        operation_id,
        params,
        body,
        responses,
        deprecated: m.doc.deprecated
            || m.anns.iter().any(|a| a.name == "Deprecated")
            || m.anns.iter().any(|a| {
                a.name == "Operation"
                    && (a.args.contains("deprecated = true") || a.args.contains("deprecated=true"))
            }),
        line: m.line,
    }]
}

/// Swagger 3 `@Tag(name = …)` / Swagger 2 `@Api(tags = {…})`.
fn class_tag_name(cls: &JavaClass) -> Option<String> {
    for a in &cls.anns {
        match a.name.as_str() {
            "Tag" => {
                if let Some(name) = ann_string(&a.args, "name") {
                    if !name.is_empty() {
                        return Some(name);
                    }
                }
            }
            "Api" => {
                if let Some(name) = ann_first_string(a) {
                    if !name.is_empty() {
                        return Some(name);
                    }
                }
            }
            _ => {}
        }
    }
    None
}
fn operation_summary(anns: &[Ann]) -> Option<String> {
    for a in anns {
        match a.name.as_str() {
            "Operation" => {
                if let Some(s) = ann_string(&a.args, "summary") {
                    return Some(s);
                }
            }
            "ApiOperation" => {
                if let Some(s) = ann_string(&a.args, "value") {
                    return Some(s);
                }
            }
            _ => {}
        }
    }
    None
}

fn operation_description(anns: &[Ann]) -> Option<String> {
    for a in anns {
        match a.name.as_str() {
            "Operation" => {
                if let Some(s) = ann_string(&a.args, "description") {
                    return Some(s);
                }
            }
            "ApiOperation" => {
                if let Some(s) = ann_string(&a.args, "notes") {
                    return Some(s);
                }
            }
            _ => {}
        }
    }
    None
}

fn schema_example(anns: &[Ann]) -> Option<String> {
    for ann in anns {
        if matches!(
            ann.name.as_str(),
            "Schema" | "ApiModelProperty" | "ApiParam" | "Parameter"
        ) {
            if let Some(value) = ann_string(&ann.args, "example") {
                if !value.is_empty() {
                    return Some(value);
                }
            }
        }
    }
    None
}
/// `@ApiResponse(code = 401, message = "…")` and Swagger 3
/// `@ApiResponse(responseCode = "401", description = "…")`.
fn api_response_entries(anns: &[Ann]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for ann in anns {
        match ann.name.as_str() {
            "ApiResponse" => {
                if let Some(entry) = parse_api_response(&ann.args) {
                    out.push(entry);
                }
            }
            "ApiResponses" => {
                let mut rest = ann.args.as_str();
                while let Some(pos) = rest.find("@ApiResponse") {
                    rest = &rest[pos + "@ApiResponse".len()..];
                    let Some(open) = rest.find('(') else { break };
                    let body = &rest[open + 1..];
                    let Some(end) = closing_paren(body) else { break };
                    if let Some(entry) = parse_api_response(&body[..end]) {
                        out.push(entry);
                    }
                    rest = &body[end..];
                }
            }
            _ => {}
        }
    }
    out
}

fn parse_api_response(args: &str) -> Option<(String, String)> {
    let code = ann_string(args, "responseCode")
        .or_else(|| ann_attr(args, "code").map(|v| v.trim().trim_matches('"').to_string()))
        .unwrap_or_else(|| "200".to_string());
    let description = ann_string(args, "description")
        .or_else(|| ann_string(args, "message"))
        .unwrap_or_default();
    Some((code, description))
}

fn closing_paren(text: &str) -> Option<usize> {
    let mut depth = 1i32;
    let mut quote: Option<char> = None;
    for (i, c) in text.char_indices() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => quote = Some(c),
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

fn response_media(anns: &[Ann], class_anns: &[Ann], ret: Option<&Ty>) -> String {
    let joined = anns
        .iter()
        .chain(class_anns.iter())
        .map(|a| a.args.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    for (needle, media) in [
        ("application/xml", "application/xml"),
        ("APPLICATION_XML", "application/xml"),
        ("TEXT_XML", "application/xml"),
        ("text/plain", "text/plain"),
        ("TEXT_PLAIN", "text/plain"),
        ("text/html", "text/html"),
        ("octet-stream", "application/octet-stream"),
    ] {
        if joined.contains(needle) {
            return media.to_string();
        }
    }
    if matches!(ret, Some(Ty::Binary)) {
        return "application/octet-stream".to_string();
    }
    "application/json".to_string()
}

/// `@PathParam("id")`: a positional string only when no `key = value` is present.
fn positional_string(a: &Ann) -> Option<String> {
    if a.args.contains('=') {
        None
    } else {
        ann_first_string(a)
    }
}
fn ann_name(a: &Ann) -> Option<String> {
    ann_string(&a.args, "value")
        .or_else(|| ann_string(&a.args, "name"))
        .or_else(|| positional_string(a))
}

/// Very small annotation-argument reader; enough for `value=`, `path=`,
/// `required=false`, `defaultValue="1"`.
fn attr_bool(args: &str, key: &str) -> Option<bool> {
    ann_attr(args, key).map(|v| v.trim() == "true")
}

fn join_path(base: &str, sub: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for p in [base, sub] {
        for seg in p.split('/') {
            if !seg.is_empty() {
                parts.push(seg);
            }
        }
    }
    if parts.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", parts.join("/"))
    }
}

fn lower_first(s: &str) -> String {
    let mut it = s.chars();
    match it.next() {
        Some(c) => c.to_lowercase().collect::<String>() + it.as_str(),
        None => String::new(),
    }
}

/// `Result<UserDto>` -> `UserDto`, `List<Long>` -> `List<Long>`.
pub fn unwrap_wrapper(raw: &str) -> String {
    let base = base_type(raw);
    if WRAPPERS.contains(&base.as_str()) {
        if let Some(inner) = generic_args(raw).first() {
            return inner.clone();
        }
    }
    raw.trim().to_string()
}

/// A single upper-case letter is treated as a Java generic type variable
/// (`T`, `E`, `K`, `V`, …). `parse_ty` maps these to `Ty::Any` unless a binding
/// supplied by an enclosing generic wrapper replaces them.
fn is_type_variable(s: &str) -> bool {
    let s = s.trim();
    s.len() == 1 && s.chars().next().is_some_and(|c| c.is_ascii_uppercase())
}

/// Whether a class looks like a response wrapper: it declares a field whose
/// type is a bare generic variable (e.g. `private T data;`) or inherits from
/// such a class (e.g. `PaginationMpResponseVO<T> extends DefaultMpResponseVO<T>`).
/// Walking bases with a depth guard avoids cycles.
fn has_generic_data_field(cls: &JavaClass, index: &TypeIndex, depth: usize) -> bool {
    if depth > 32 {
        return false;
    }
    if cls.fields.iter().any(|f| is_type_variable(&f.ty)) {
        return true;
    }
    for base in &cls.bases {
        if let Some(key) = index.key_for(cls, base) {
            if let Some(base_cls) = index.get(&key) {
                if has_generic_data_field(base_cls, index, depth + 1) {
                    return true;
                }
            }
        }
    }
    false
}

fn base_type(raw: &str) -> String {
    let t = raw.trim();
    let t = t.split('<').next().unwrap_or(t);
    t.rsplit('.').next().unwrap_or(t).trim().to_string()
}

pub fn generic_args(raw: &str) -> Vec<String> {
    let Some(open) = raw.find('<') else { return Vec::new() };
    let Some(close) = raw.rfind('>') else { return Vec::new() };
    if close <= open {
        return Vec::new();
    }
    split_top_level(&raw[open + 1..close], ',')
}

fn is_simple(raw: &str, index: &TypeIndex, ctx: &JavaClass) -> bool {
    let base = base_type(raw);
    if index.resolve(ctx, &base).is_some() {
        return false;
    }
    !matches!(
        base.as_str(),
        "List" | "Set" | "Collection" | "Map" | "HashMap" | "Object" | "T" | "?"
    )
}

fn is_complex(raw: &str, index: &TypeIndex, ctx: &JavaClass) -> bool {
    !is_simple(raw, index, ctx)
}

/// Resolves a Java type expression into the schema model.
pub fn parse_ty(raw: &str, index: &TypeIndex, ctx: &JavaClass, unwrap: bool) -> Ty {
    parse_ty_bound(raw, index, ctx, unwrap, &HashMap::new(), &mut Vec::new())
}

/// Core of `parse_ty` with generic type-variable bindings in scope.
/// `bindings` maps a bare type variable (`T`) to a concrete type expression
/// (`MpStlmInfoOutput`) supplied by an enclosing wrapper such as
/// `DefaultMpResponseVO<MpStlmInfoOutput>`.
fn parse_ty_bound(
    raw: &str,
    index: &TypeIndex,
    ctx: &JavaClass,
    unwrap: bool,
    bindings: &HashMap<String, String>,
    path: &mut Vec<String>,
) -> Ty {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ty::Any;
    }
    // A bare type variable resolves through the enclosing wrapper's binding.
    if is_type_variable(raw) {
        if let Some(bound) = bindings.get(raw) {
            return parse_ty_bound(bound, index, ctx, false, bindings, path);
        }
    }
    if unwrap {
        let known = WRAPPERS.contains(&base_type(raw).as_str());
        if known {
            let inner = unwrap_wrapper(raw);
            if inner != raw {
                return parse_ty_bound(&inner, index, ctx, false, bindings, path);
            }
        }
    }
    if let Some(stripped) = raw.strip_suffix("[]") {
        return Ty::Array(Box::new(parse_ty_bound(
            stripped, index, ctx, false, bindings, path,
        )));
    }
    let base = base_type(raw);
    let args = generic_args(raw);
    match base.as_str() {
        "String" | "CharSequence" | "char" | "Character" | "UUID" => Ty::String,
        "int" | "Integer" | "short" | "Short" | "byte" | "Byte" | "BigInteger" => Ty::Integer,
        "long" | "Long" => Ty::Long,
        "double" | "Double" | "float" | "Float" | "BigDecimal" | "Number" => Ty::Number,
        "boolean" | "Boolean" => Ty::Boolean,
        "MultipartFile" | "File" | "Path" | "Resource" | "InputStream" => Ty::Binary,
        "List" | "Set" | "Collection" | "Iterable" | "Queue" | "Deque" | "Stream" | "ArrayList"
        | "LinkedList" | "HashSet" => match args.first() {
            Some(a) => Ty::Array(Box::new(parse_ty_bound(a, index, ctx, false, bindings, path))),
            None => Ty::Array(Box::new(Ty::Any)),
        },
        "Optional" => match args.first() {
            Some(a) => parse_ty_bound(a, index, ctx, false, bindings, path),
            None => Ty::Any,
        },
        b if PAGE_TYPES.contains(&b) => {
            let item = args
                .first()
                .map(|a| parse_ty_bound(a, index, ctx, false, bindings, path))
                .unwrap_or(Ty::Any);
            Ty::Object(vec![
                Field::new("records", Ty::Array(Box::new(item)), true, "当前页数据"),
                Field::new("total", Ty::Long, true, "总条数"),
                Field::new("size", Ty::Long, false, "每页条数"),
                Field::new("current", Ty::Long, false, "当前页"),
                Field::new("pages", Ty::Long, false, "总页数"),
            ])
        }
        "Map" | "HashMap" | "JSONObject" | "Object" | "T" | "?" => Ty::Any,
        b if index.resolve(ctx, b).is_some() => {
            // Generic response wrapper (e.g. `DefaultMpResponseVO<T>` with a
            // `data: T` field): inline it, substituting each type variable with
            // the corresponding concrete argument so nested DTOs keep their
            // fields instead of collapsing to `{}`.
            if !args.is_empty() {
                if let Some(key) = index.key_for(ctx, b) {
                    if let Some(cls) = index.get(&key) {
                        if has_generic_data_field(cls, index, 0) {
                            let vars = generic_vars_of(cls, index, 0);
                            let mut bound = bindings.clone();
                            for (i, var) in vars.iter().enumerate() {
                                if let Some(arg) = args.get(i) {
                                    bound.insert(var.clone(), arg.clone());
                                }
                            }
                            // The returned Object carries the concrete DTO as
                            // a nested `Ty::Ref`, so collect_schemas will pick
                            // it up from the response type via collect_refs.
                            let mut sink = Vec::new();
                            let inline = class_schema_bound(
                                cls, index, &mut sink, path, &bound,
                            );
                            if let Ty::Object(_) = &inline {
                                return inline;
                            }
                        }
                    }
                }
            }
            Ty::Ref(index.key_for(ctx, b).unwrap_or_else(|| b.to_string()))
        }
        _ => Ty::Any,
    }
}

/// Collect the distinct bare type variables used by a class and its bases,
/// in declaration order (fields first, then inherited ones).
fn generic_vars_of(cls: &JavaClass, index: &TypeIndex, depth: usize) -> Vec<String> {
    let mut vars: Vec<String> = Vec::new();
    if depth > 32 {
        return vars;
    }
    for f in &cls.fields {
        let t = f.ty.trim();
        if is_type_variable(t) && !vars.iter().any(|v| v == t) {
            vars.push(t.to_string());
        }
    }
    for base in &cls.bases {
        if let Some(key) = index.key_for(cls, base) {
            if let Some(base_cls) = index.get(&key) {
                for v in generic_vars_of(base_cls, index, depth + 1) {
                    if !vars.iter().any(|x| x == &v) {
                        vars.push(v);
                    }
                }
            }
        }
    }
    vars
}

fn collect_schemas(endpoints: &[Endpoint], index: &TypeIndex) -> BTreeMap<String, Ty> {
    let mut schemas = BTreeMap::new();
    let mut queue: Vec<String> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();

    for ep in endpoints {
        for p in &ep.params {
            collect_refs(&p.ty, &mut queue);
        }
        if let Some((_, ty)) = &ep.body {
            collect_refs(ty, &mut queue);
        }
        for response in &ep.responses {
            if let Some(ty) = &response.ty {
                collect_refs(ty, &mut queue);
            }
        }
    }
    while let Some(name) = queue.pop() {
        if !seen.insert(name.clone()) {
            continue;
        }
        let Some(cls) = index.get(&name) else { continue };
        let ty = class_schema(cls, index, &mut queue, &mut Vec::new());
        schemas.insert(name, ty);
    }
    schemas
}

fn collect_refs(ty: &Ty, out: &mut Vec<String>) {
    match ty {
        Ty::Ref(n) => out.push(n.clone()),
        Ty::Array(inner) => collect_refs(inner, out),
        Ty::Object(fields) => {
            for f in fields {
                collect_refs(&f.ty, out);
            }
        }
        _ => {}
    }
}

fn class_schema(
    cls: &JavaClass,
    index: &TypeIndex,
    queue: &mut Vec<String>,
    path: &mut Vec<String>,
) -> Ty {
    class_schema_bound(cls, index, queue, path, &HashMap::new())
}

/// Like `class_schema`, but resolves field types with generic `bindings` in
/// scope so a wrapper's `data: T` becomes the concrete response DTO.
fn class_schema_bound(
    cls: &JavaClass,
    index: &TypeIndex,
    queue: &mut Vec<String>,
    path: &mut Vec<String>,
    bindings: &HashMap<String, String>,
) -> Ty {
    if !cls.enum_values.is_empty() {
        return Ty::Enum(cls.enum_values.clone());
    }
    if path.iter().any(|n| n == &cls.name) {
        return Ty::Object(Vec::new());
    }
    path.push(cls.name.clone());
    // Superclass fields come first, like the reference implementation.
    let mut fields: Vec<Field> = Vec::new();
    for base in &cls.bases {
        let Some(key) = index.key_for(cls, base) else { continue };
        let Some(base_cls) = index.get(&key) else { continue };
        if let Ty::Object(base_fields) =
            class_schema_bound(base_cls, index, queue, path, bindings)
        {
            for f in base_fields {
                if !fields.iter().any(|x| x.name == f.name) {
                    fields.push(f);
                }
            }
        }
    }
    for f in &cls.fields {
        if is_hidden(f) {
            continue;
        }
        let name = field_name(f);
        if fields.iter().any(|x| x.name == name) {
            continue;
        }
        let ty = parse_ty_bound(&f.ty, index, cls, false, bindings, path);
        collect_refs(&ty, queue);
        let required = !PRIMITIVE_REQUIRED.contains(&base_type(&f.ty).as_str())
            && f.anns.iter().any(|a| {
                matches!(
                    a.name.as_str(),
                    "NotNull" | "NotBlank" | "NotEmpty" | "NonNull"
                )
            });
        let description = if f.doc.is_empty() {
            schema_description(&f.anns).unwrap_or_default()
        } else {
            f.doc.clone()
        };
        let mut field = Field::new(name, ty, required, description);
        if let Some(example) = schema_example(&f.anns) {
            field.example = example;
        }
        apply_constraints(&mut field, &f.anns);
        fields.push(field);
    }
    path.pop();
    Ty::Object(fields)
}

/// `@JsonProperty("user_name")` / `@JSONField(name = …)` / `@SerializedName` /
/// `@Schema(name = …)` rename the JSON property.
fn field_name(f: &JavaField) -> String {
    for a in &f.anns {
        let key = match a.name.as_str() {
            "JsonProperty" | "SerializedName" | "JSONField" => "value",
            "Schema" | "ApiModelProperty" => "name",
            _ => continue,
        };
        if let Some(v) = ann_string(&a.args, key) {
            if !v.is_empty() {
                return v;
            }
        }
        if matches!(a.name.as_str(), "JsonProperty" | "SerializedName" | "JSONField") {
            if let Some(v) = ann_first_string(a) {
                if !v.is_empty() {
                    return v;
                }
            }
        }
    }
    f.name.clone()
}

/// Bean Validation -> JSON Schema keywords.
fn apply_constraints(field: &mut Field, anns: &[Ann]) {
    // `@Min(1)` has a positional value, `@Min(value = 1)` a named one.
    let arg = |a: &Ann, key: &str| {
        ann_attr(&a.args, key)
            .map(|v| v.trim().trim_matches('"').to_string())
            .or_else(|| {
                let t = a.args.trim();
                if t.is_empty() || t.contains('=') {
                    None
                } else {
                    Some(t.trim_matches('"').to_string())
                }
            })
    };
    let num = |a: &Ann, key: &str| arg(a, key).and_then(|v| v.parse::<f64>().ok());
    let int = |a: &Ann, key: &str| arg(a, key).and_then(|v| v.parse::<u64>().ok());
    for a in anns {
        match a.name.as_str() {
            "Min" => field.min = num(a, "value"),
            "Max" => field.max = num(a, "value"),
            "DecimalMin" => field.min = num(a, "value"),
            "DecimalMax" => field.max = num(a, "value"),
            "Size" => {
                field.min_len = int(a, "min");
                field.max_len = int(a, "max");
            }
            "Length" => {
                field.min_len = int(a, "min");
                field.max_len = int(a, "max");
            }
            "Pattern" => {
                field.pattern = ann_string(&a.args, "regexp")
                    .or_else(|| ann_first_string(a))
                    .map(|p| unescape_java(p.as_str()));
            }
            "Email" => field.format = Some("email".into()),
            _ => {}
        }
    }
}

/// Java string escapes in annotation values: `"^\\d+$"` means `^\d+$`.
fn unescape_java(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}
fn is_hidden(f: &JavaField) -> bool {
    let base = base_type(&f.ty);
    if base.contains("serialVersionUID") || f.name == "serialVersionUID" {
        return true;
    }
    if f.anns.iter().any(|a| a.name == "JsonIgnore") {
        return true;
    }
    // `@JSONField(name = "x")` renames; only `serialize = false` hides.
    if f.anns.iter().any(|a| {
        a.name == "JSONField"
            && (a.args.contains("serialize = false") || a.args.contains("serialize=false"))
    }) {
        return true;
    }
    f.anns.iter().any(|a| {
        matches!(a.name.as_str(), "Schema" | "ApiModelProperty")
            && (a.args.contains("hidden = true") || a.args.contains("hidden=true"))
    })
}

fn schema_description(anns: &[Ann]) -> Option<String> {
    for a in anns {
        if matches!(a.name.as_str(), "Schema" | "ApiModelProperty") {
            if let Some(d) = ann_string(&a.args, "description").or_else(|| ann_string(&a.args, "value")) {
                return Some(d);
            }
        }
    }
    None
}