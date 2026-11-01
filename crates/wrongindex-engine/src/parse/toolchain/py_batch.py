import ast, json, sys

# Decorators whose function is an attribute accessor, by last dotted segment.
PROPERTY_DECORATORS = ("property", "cached_property", "abstractproperty", "setter", "getter", "deleter")

def get_name(node):
    if isinstance(node, ast.Name):
        return node.id
    if isinstance(node, ast.Attribute):
        return get_name(node.value) + "." + node.attr
    if isinstance(node, ast.Subscript):
        return get_name(node.value)
    if isinstance(node, ast.Call):
        return get_name(node.func)
    if isinstance(node, ast.Constant):
        return str(node.value)
    return ""

def leaf_name(node):
    # Declared name of the callee: `join` of `os.path.join`. Matches how the
    # TypeScript and Go extractors record call refs.
    if isinstance(node, ast.Attribute):
        return node.attr
    if isinstance(node, ast.Name):
        return node.id
    return get_name(node).split(".")[-1]

def is_private(name):
    return name.startswith("__") and not name.endswith("__")

def first_doc_line(node):
    try:
        doc = ast.get_docstring(node)
    except Exception:
        return ""
    for line in (doc or "").splitlines():
        line = line.strip()
        if line:
            return line[:200]
    return ""

def module_name_of(path):
    base = path.replace("\\", "/").rsplit("/", 1)[-1]
    return base.rsplit(".", 1)[0] if "." in base else base

def read_stdin_text():
    # Decode explicitly: sys.stdin uses the locale code page on Windows
    # (cp1254, cp1252, ...), which mangles or rejects the UTF-8 we are sent.
    return sys.stdin.buffer.read().decode("utf-8", "replace")

def parse_source(text, name):
    # ast.parse rejects a leading U+FEFF in a str source.
    if text.startswith("\ufeff"):
        text = text[1:]
    return ast.parse(text, filename=name or "<module>")

def format_args(args):
    parts = [a.arg for a in list(getattr(args, "posonlyargs", [])) + list(args.args)]
    if args.vararg is not None:
        parts.append("*" + args.vararg.arg)
    elif args.kwonlyargs:
        parts.append("*")
    parts.extend(a.arg for a in args.kwonlyargs)
    if args.kwarg is not None:
        parts.append("**" + args.kwarg.arg)
    return ", ".join(parts)

def extract(tree, module_name):
    syms = []
    refs = []
    scope = [module_name]

    def add(name, kind, line, col, signature, sym_scope, doc=""):
        syms.append({
            "name": name, "kind": kind, "line": line, "col": col,
            "signature": signature, "scope": sym_scope, "doc": doc,
        })

    def qualified(name):
        return ".".join(scope) + "." + name

    # Kinds are the index schema's (class/function/method/property/var/const/
    # type). Imports are refs, not symbols: an import symbol per file made
    # every imported name look like a definition in every importing module.
    class Visitor(ast.NodeVisitor):
        in_class = False

        def visit_ClassDef(self, node):
            bases = [get_name(b) for b in node.bases]
            sig = "class " + node.name + ("(" + ", ".join(bases) + ")" if bases else "") + ": ..."
            add(node.name, "class", node.lineno, node.col_offset, sig, qualified(node.name), first_doc_line(node))
            scope.append(node.name)
            outer = self.in_class
            self.in_class = True
            try:
                self.generic_visit(node)
            finally:
                self.in_class = outer
                scope.pop()

        def visit_FunctionDef(self, node):
            kind = "method" if self.in_class else "function"
            for dec in node.decorator_list:
                if get_name(dec).rsplit(".", 1)[-1] in PROPERTY_DECORATORS:
                    kind = "property"
            prefix = "async def " if isinstance(node, ast.AsyncFunctionDef) else "def "
            sig = prefix + node.name + "(" + format_args(node.args) + ")"
            returns = get_name(node.returns) if node.returns is not None else ""
            if returns:
                sig += " -> " + returns
            add(node.name, kind, node.lineno, node.col_offset, sig, qualified(node.name), first_doc_line(node))
            # Bodies are not visited: function locals are not index symbols.

        visit_AsyncFunctionDef = visit_FunctionDef

        def add_target(self, target, line, annotation=None, has_value=True):
            if isinstance(target, ast.Name):
                if is_private(target.id):
                    return
                kind = "const" if target.id.isupper() else "var"
                if annotation is not None:
                    sig = target.id + ": " + get_name(annotation) + (" = ..." if has_value else "")
                else:
                    sig = target.id + " = ..."
                add(target.id, kind, line, target.col_offset, sig, ".".join(scope))
            elif isinstance(target, (ast.Tuple, ast.List)):
                for elt in target.elts:
                    self.add_target(elt, line)
            elif isinstance(target, ast.Starred):
                self.add_target(target.value, line)

        def visit_Assign(self, node):
            for target in node.targets:
                self.add_target(target, node.lineno)

        def visit_AnnAssign(self, node):
            self.add_target(node.target, node.lineno, node.annotation, node.value is not None)

        def visit_TypeAlias(self, node):
            if isinstance(node.name, ast.Name):
                add(node.name.id, "type", node.lineno, node.col_offset, "type " + node.name.id + " = ...", ".".join(scope))

    Visitor().visit(tree)

    # Refs need a full walk: the visitor deliberately skips function bodies,
    # which is exactly where the calls are.
    for node in ast.walk(tree):
        if isinstance(node, ast.Call):
            name = leaf_name(node.func)
            if name:
                refs.append({"toName": name, "callType": "call", "line": node.lineno})
        elif isinstance(node, ast.Import):
            for alias in node.names:
                refs.append({
                    "toName": alias.name.split(".")[-1], "callType": "import",
                    "line": node.lineno, "module": alias.name,
                })
        elif isinstance(node, ast.ImportFrom):
            # PEP 328: keep the leading dots so the resolver can walk up from
            # the importing file's package.
            module = ("." * (node.level or 0)) + (node.module or "")
            for alias in node.names:
                refs.append({
                    "toName": alias.name, "callType": "import",
                    "line": node.lineno, "module": module,
                })
        elif isinstance(node, ast.ClassDef):
            for base in node.bases:
                name = leaf_name(base)
                if name:
                    refs.append({"toName": name, "callType": "inherit", "line": node.lineno})

    return syms, refs

def main():
    try:
        inputs = json.loads(read_stdin_text())
    except Exception:
        print(json.dumps({"version": 1, "results": []}))
        return
    results = []
    for entry in inputs:
        name = entry.get("file", "")
        result = {"file": name, "symbols": [], "refs": []}
        try:
            syms, refs = extract(parse_source(entry.get("content", ""), name), module_name_of(name))
            result["symbols"] = syms
            result["refs"] = refs
        except Exception as e:
            result["error"] = str(e) or type(e).__name__
        results.append(result)
    print(json.dumps({"version": 1, "results": results}))

main()
