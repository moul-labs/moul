"""Rule edge cases the Go engine's own tests never tried.

export.py asks the Go engine about each case and writes its answers to
crates/moul-rules/tests/data. Each function returns its cases in the order they
are written; keep the order when adding cases so the diff stays readable.
Cases must not depend on the clock: a rule may read @now, but its answer must
not change with the time of day.
"""


def evaluate():
    """go_edge_cases.jsonl: rules.EvaluateRule without a database.

    What expr-lang decides (type errors met while checking or running,
    constant folding, tokens glued by odd spacing, and the messages and
    positions of each), then text that Go's ParseFloat reads as a number or
    not (out of range, `_`, a signed NaN).
    """
    rec ={"id":"u1","title":"Hello World","status":"active","price":10,"tags":["a","b"],"owner":"u1","nothing":None,
           "meta":{"k":"v","n":2},"flag":True,"count":"7","role":"user","createdAt":"2001-01-01 00:00:00.000Z"}
    def c(rule, auth=None, record=rec, context=None):
        return {"rule": rule, "auth": auth, "record": record, "context": context}
    body = lambda b: {"body": b}
    cases = [
     c("status = 1"), c("price = '10'"), c("price = 10"), c("price > 5"), c("price > '5'"), c("count > 5"),
     c("title"), c("nothing"), c("missing"), c("nil"), c("missing = null"), c("nothing = null"), c("nothing = 'x'"),
     c("nothing > 1"), c("tags = 'a'"), c("tags ?= 'a'"), c("tags ?!= 'a'"), c("tags:each != 'c'"), c("tags:length > 1"),
     c("title:length = 11"), c("meta.k = 'v'"), c("meta.n = 2"), c("meta.missing.x = 1"), c("nothing.x = 1"),
     c("tags.x = 1"), c("title.x = 1"),
     c("@request.body.a.b = 1", context=body({})), c("@request.body.title", context=body({"title":"x"})),
     c("@request.body.flag && true", context=body({"flag":"yes"})),
     c("@request.method = 'GET'", context={"method":"GET"}),
     c("@request.auth.id = owner", auth={"id":"u1"}), c("@request.auth.id = ''"), c("auth.id = owner", auth={"id":"u1"}),
     c("@request.query.page > 2", context={"query":{"page":"3"}}), c("@request.query.page = '3'", context={"query":{"page":"3"}}),
     c("lower(title) = 'hello world'"), c("title:lower ~ 'WORLD'"), c("title !~ 'xyz'"), c("title ~ 'h_llo%'"),
     c("title ~ ''"), c("price ~ '1'"), c("price - 5 = 5"), c("price -5 = 5"), c("a-b = 1"),
     c("tags contains 'a'"), c("title contains 'World'"), c("'a' in tags"), c("not flag"),
     c("flag = true && price > 5 || false"), c("(flag = true)"), c("flag = true AND (status = 'x' OR price >= 10)"),
     c("exists_group_x"), c("get('users', owner) = null"), c("exists('posts', 'id = ?', owner)"),
     c("geoDistance(0, 0, 0, 1) > 111"), c("geoDistance('a', 0, 0, 1) = 0"), c("length(tags) = 2"),
     c("isset(meta, 'k')"), c("lower(1)"), c("lower()"), c("lower(1, 2) = ''"), c("exists(1, 'x')"), c("exists(nothing, 'x')"),
     c("exists(@request.body.t, 'x')", context=body({})), c("exists(@request.body.t, 'x')", context=body({"t":5})),
     c("unknownFn(1)"), c("title(1)"), c("nothing(1)"), c("price = 10.5"), c("price = 1.2.3"), c("5abc = 1"),
     c("title = \"it's\""), c("title = 'tab\\there'"), c("title = 'line\nbreak'"),
     c("@collection.posts.title = 'x'"), c("@collection.posts.title = 'x' || true"), c("@collection.posts.title ?= 'x'"),
     c("title == 'Hello World'"), c("price >= 10 && price <= 10"), c("5 < price < 20"), c("title = @now"),
     c("createdAt < @yesterday"), c("tags:each > 0"), c("tags ?> 0"),
     c("@request.body.role:changed = true", context=body({"role":"admin"})),
     c("@request.body.role:changed = true", context=body({"role":"user"})),
     c("@request.body.role:isset", context=body({"role":"admin"})), c("meta:isset"), c("title:upper = 'x'"),
     c("title in ('a', 'b')"), c("!flag"), c("title != ''"), c("price = -10"), c("flag = 1"), c("meta = null"),
     c("tags = null"), c("flag ?= true"), c("count ?> 5"), c("title ~ @request.query.q", context={"query":{"q":"WORLD"}}),
     c("status = 'active' // comment"), c("status = 'active' and"), c("   "), c("// just a comment"),
     c("title ?= @request.body.list", context=body({"list":["x"]})), c("@request.body.list ?= 'x'", context=body({"list":["x"]})),
     c("@request.headers.x = 'y'", context={"headers":{"x":"y"}}),
     c("id = @request.auth.id", auth={"id":"u1"}), c("true && 'x'"), c("@request.body.x && true", context=body({})),
     c("@request.body.x || @request.body.y", context=body({})), c("meta.n > 1 && meta.n < 3"), c("tags:each = 'a'"),
     c("price ?= 10"), c("nothing:length = 0"), c("missing:lower = ''"), c("title = 'x' = false"), c("geoDistance(0,0)"),
     c("strftime()"), c("all_eq(tags, 'a')"), c("title matches 'H.*'"), c("(title)contains('World')"),
     c("'abc' contains 'b'"), c("(tags)contains('a')"), c("('a')in(tags)"), c("not(flag)"), c("(price)..(5)"),
     c("meta.k ?= 'v'"), c("tags:each ~ @request.body.p", context=body({"p":"a"})), c("@request.auth.missing.x = 1", auth={"id":"u1"}),
     c("title = 'Hello World' && meta"), c("flag && title"), c("price = price"), c("tags = tags"), c("meta = meta"),
     c("title = `x`"), c("title = 'x' ?"), c("@request.body.n = 1", context=body({"n":1})),
     c("@request.body.n > 0.5", context=body({"n":1})), c("@request.body.s < 'b'", context=body({"s":"a"})),
     c("@request.body.b < true", context=body({"b":False})), c("lower(@request.body.n) = '1e+06'", context=body({"n":1000000})),
     c("@request.body.n ?= '1e+06'", context=body({"n":1000000})), c("@request.body.v = 'map[a:1]'", context=body({"v":{"a":1}})),
     c("lower(@request.body.v) = 'map[a:1 b:[x true]]'", context=body({"v":{"b":["x",True],"a":1}})),
     c("@request.body.v ?= 'x'", context=body({"v":None})), c("@request.body.v:length = 0", context=body({"v":None})),
     c("title:each = 'Hello World'"), c("missing:each = 'x'"), c("@request.body.a.b:isset", context=body({"a":{"b":1}})),
     c("@request.body.a.b:isset", context=body({})), c("geoDistance(@request.body.a, 0, 0, 0) = 0", context=body({"a":"0"})),
     c("title = 'Hello World' || nothing.x"), c("title = 'nope' && nothing.x"), c("_record.title = 'Hello World'"),
     c("request.auth.id = ''"), c("lower = 'x'"), c("@yearStart < @yearEnd"), c("@todayStart <= @now && @now <= @todayEnd"),
    ]

    # Numbers parsed from text.
    def add(rule, record, body):
        cases.append({"rule": rule, "auth": None, "record": record, "context": {"body": body}})
    add("@request.body.qty ?<= 100", {}, {"qty": "-1e999"})
    add("@request.body.qty ?> 5", {}, {"qty": "1e400"})
    add("@request.body.qty:each < 0", {}, {"qty": ["-1e999"]})
    add("@request.body.qty:each < 0", {}, {"qty": ["-1e999", "-1"]})
    add("code ?= @request.body.code", {"code": "1e400"}, {"code": "1e500"})
    add("code ?= @request.body.code", {"code": "1e400"}, {"code": "1e400"})
    add("code ?!= @request.body.code", {"code": "1e400"}, {"code": "1e500"})
    add("@request.body.qty ?> 5", {}, {"qty": "Infinity"})
    add("@request.body.qty ?< 0", {}, {"qty": "-inf"})
    add("@request.body.qty ?= 1000", {}, {"qty": "1_000"})
    add("@request.body.qty ?= 1000", {}, {"qty": "1__000"})
    add("@request.body.qty ?= 1000", {}, {"qty": "_1000"})
    add("@request.body.qty ?!= 0", {}, {"qty": "-nan"})
    add("@request.body.qty ?!= 0", {}, {"qty": "NaN"})
    add("@request.body.qty ?= 0", {}, {"qty": "1e-400"})
    add("code ?= @request.body.code", {"code": "-nan"}, {"code": "-nan"})
    add("code ?= @request.body.code", {"code": "NaN"}, {"code": "NaN"})
    add("@request.body.qty ?> 1", {}, {"qty": "1.7976931348623159e308"})
    add("@request.body.role:changed = true", {"role": "1e400"}, {"role": "1e500"})
    return cases


def evaluate_db():
    """go_edge_cases_db.jsonl: rules.EvaluateRule with a database.

    Returns the SQL that builds the database (modernc.org/sqlite, fresh for
    each case) and the cases, which probe get(, exists(, strftime( and
    @collection: how values are bound and read back.
    """
    setup = [
     "CREATE TABLE _moul (id TEXT PRIMARY KEY, name TEXT UNIQUE NOT NULL, type TEXT NOT NULL, fields TEXT NOT NULL, rules TEXT NOT NULL, createdAt TEXT NOT NULL, updatedAt TEXT NOT NULL)",
     "CREATE TABLE posts (id TEXT PRIMARY KEY, title TEXT, author TEXT, views NUMERIC, published INTEGER, meta TEXT, tags TEXT, editors TEXT, passwordHash TEXT, score REAL, data BLOB)",
     "INSERT INTO posts VALUES ('p1', 'Hello World', 'u1', 10, 1, '{\"a\":1,\"b\":[true,null]}', '[\"x\",\"y\"]', '[\"u1\",\"u2\"]', 'secret', 1.5, X'6869')",
     "INSERT INTO posts VALUES ('p2', 'Second 100%', 'u2', 0, 0, '', NULL, NULL, NULL, NULL, NULL)",
     "INSERT INTO posts VALUES ('p3', 'Third', 'u3', 2.5, 'yes', 'not json', 'null', 'null', NULL, 1e21, '')",
     "CREATE TABLE user_roles (id TEXT PRIMARY KEY, user_id TEXT, role TEXT, level INTEGER)",
     "INSERT INTO user_roles VALUES ('r1', 'u1', 'admin', 5)",
     "INSERT INTO user_roles VALUES ('r2', 'u2', 'editor', 2)",
     "INSERT INTO _moul VALUES ('m1', 'posts', 'base', '[{\"name\":\"title\",\"type\":\"text\"},{\"name\":\"author\",\"type\":\"relation\",\"relationConfig\":{\"targetMoul\":\"users\",\"cardinality\":\"1:N\"}},{\"name\":\"views\",\"type\":\"number\"},{\"name\":\"published\",\"type\":\"bool\"},{\"name\":\"meta\",\"type\":\"json\"},{\"name\":\"tags\",\"type\":\"json\"},{\"name\":\"editors\",\"type\":\"relation\",\"relationConfig\":{\"targetMoul\":\"users\",\"cardinality\":\"M:N\"}},{\"name\":\"score\",\"type\":\"number\"}]', '{}', '', '')",
    ]
    rec = {"id": "u2", "createdAt": "2001-01-01 00:00:00.000Z"}
    def c(rule, auth=None, record=rec, context=None):
        return {"rule": rule, "auth": auth, "record": record, "context": context}
    body = lambda b: {"body": b}
    cases = [
     c("get('posts', 'p1') = null"), c("get('posts', 'nope') = null"), c("isset(get('posts', 'p1'), 'title')"),
     c("isset(get('posts', 'p1'), 'passwordHash')"), c("length(get('posts', 'p1')) = 10"), c("length(get('posts', 'p1')) = 11"),
     c("lower(get('posts', 'p1')) = ''"), c("lower(get('posts', 'p2')) = ''"), c("lower(get('posts', 'p3')) = ''"),
     c("lower(get('posts', 'p1')) = 'map[author:u1 data:hi editors:[u1 u2] id:p1 meta:map[a:1 b:[true <nil>]] published:true score:1.5 tags:[x y] title:hello world views:10]'"),
     c("lower(get('posts', 'p2')) = 'map[author:u2 data:<nil> editors:[] id:p2 meta: published:false score:<nil> tags:<nil> title:second 100% views:0]'"),
     c("lower(get('posts', 'p3')) = 'map[author:u3 data: editors:[] id:p3 meta:not json published:false score:1e+21 tags:<nil> title:third views:2.5]'"),
     c("get('user_roles', 'r1') = null"), c("get('posts', 1) = null"), c("get('_moul', 'm1') = null"),
     c("get('posts', @request.auth.id) = null", auth={"id": "p1"}), c("get(1, 'p1')"), c("get('posts', '') = null"),
     c("exists('posts', 'id = ?', 'p1')"), c("exists('posts', 'id = ? AND author = ?', 'p1')"), c("exists('posts', 'views > ?', 5)"),
     c("exists('posts', 'views > ?', '5')"), c("exists('posts', 'nosuchcol = 1')"), c("exists('nosuch', '1 = 1')"),
     c("exists('posts', '1 = 1')"), c("exists('posts', 'title = ?', true)"), c("exists('posts', 'published = ?', true)"),
     c("exists('posts', 'score = ?', 1.5)"), c("exists('posts', 'title LIKE ?', '%World')"),
     c("exists('posts', 'id = ?', @request.body.list)", context=body({"list": ["p1"]})),
     c("exists('posts', 'id = ?', @request.body.n)", context=body({"n": 1})), c("exists('posts', 'views = ?', @request.body.n)", context=body({"n": 10})),
     c("exists('posts', 'id = ?', id)"), c("exists('posts', 'title = ''Hello World''')"), c("exists('posts', 'title = ?')"),
     c("exists('posts', 'id = ?', 'p1', 'extra')"), c("exists('_moul', '1 = 1')"),
     c("@collection.user_roles.user_id = @request.auth.id && @collection.user_roles.role = 'admin'", auth={"id": "u1"}),
     c("@collection.user_roles.level > 3"), c("@collection.user_roles.level = 5"), c("@collection.user_roles.level >= 6"),
     c("@collection.user_roles.role ~ 'dmi'"), c("@collection.user_roles.role ~ 'adm%'"), c("@collection.user_roles.role ~ 'min%'"),
     c("@collection.user_roles.role !~ 'admin'"), c("@collection.user_roles.role ?= 'editor'"), c("@collection.user_roles.role ?!= 'editor'"),
     c("@collection.user_roles.user_id = id"), c("@collection.user_roles.user_id = missing"),
     c("@collection.user_roles.user_id = @request.body.ids", context=body({"ids": ["u1"]})),
     c("@collection.user_roles:a.role = 'admin' && @collection.user_roles:b.role = 'editor'"),
     c("@collection.user_roles.role = 'admin' && @collection.user_roles.user_id = 'u2'"),
     c("@collection.nosuch.x = 1"), c("@collection.user_roles.nosuchcol = 1"), c("@collection.user_roles.role = true"),
     c("@collection.user_roles.role = @request.auth.role", auth={"id": "u9", "role": "admin"}),
     c("@collection.user_roles.role = lower"), c("@collection._moul.name = 'posts'"),
     c("@collection.user_roles.user_id = @request.body.u", context=body({"u": {"id": "u1"}})),
     c("@collection.user_roles.level = @request.body.n", context=body({"n": 5})),
     c("@collection.user_roles.user_id = @request.body.users.id", context=body({"users": [{"id": "u2"}, {"x": 1}]})),
     c("@collection.posts.views = 2.5"), c("@collection.user_roles.user_id != 'u1' && id = 'u2'"),
     c("@collection.user_roles.role = 'admin' || @collection.user_roles.role = 'nobody'"),
     c("@collection.user_roles.role.x = 'admin'"), c("@collection.user_roles.role = 'admin' = true"),
     c("strftime('%Y-%m', '2026-07-19 12:34:56') = '2026-07'"), c("strftime('%Y', createdAt) = '2001'"),
     c("strftime('%s', 0, 'unixepoch') = '0'"), c("strftime('%Y', 'garbage') = ''"), c("strftime(1, 'x') = ''"),
     c("strftime('%Y', @request.body.list) = ''", context=body({"list": [1]})), c("strftime('%j', '2026-12-31') = '365'"),
     c("strftime('%Y-%m-%d', '2026-01-31', '+1 month') = '2026-03-03'"), c("strftime('%H', 1.5, 'unixepoch') = '00'"),
     c("strftime('%Y') = ''"), c("strftime(nothing, 'x') = ''"),
    ]
    return setup, cases


def sql():
    """go_sql_edge_cases.jsonl: BuildFilterSQLWithOptions and
    BuildSortSQLWithOptions over three collections, trusted and not.

    A filter comparing with @now is left out: its SQL parameter is the time.
    """
    def rel(t, card="1:N"): return {"targetMoul": t, "cardinality": card}
    users = {"name":"users","type":"auth","fields":[{"name":"displayName","type":"text"},{"name":"age","type":"number"}],"rules":{"listRule":""}}
    teams = {"name":"teams","type":"base","fields":[{"name":"owner","type":"text"}],"rules":{"listRule":"owner = @request.auth.id"}}
    posts = {"name":"posts","type":"base","fields":[
      {"name":"title","type":"text"},{"name":"body","type":"text"},{"name":"price","type":"number"},{"name":"active","type":"bool"},
      {"name":"tags","type":"json"},{"name":"team","type":"relation","relationConfig":rel("teams")},
      {"name":"editors","type":"relation","relationConfig":rel("users","M:N")},{"name":"author","type":"relation","relationConfig":rel("users","1:1")},
      {"name":"ssn","type":"cloak","searchable":True},{"name":"notes","type":"cloak"},{"name":"parent","type":"relation","relationConfig":rel("posts")},
      {"name":"weird","type":"relation","relationConfig":rel("posts","X:Y")},{"name":"ghost","type":"relation","relationConfig":rel("ghosts")},
      {"name":"bad","type":"relation","relationConfig":rel("_bad")},{"name":"loose","type":"relation"}],
      "rules":{"listRule":"team.owner = @request.auth.id"}}
    loader = {"users": users, "teams": teams, "posts": posts}
    auth = {"id":"u1","age":30,"flag":True,"obj":{"a":1},"nested":{"id":"zz"},"big":1000000,"nul":None}
    ctx = {"query":{"q":"hello","n":5},"headers":{"x_h":"hv"},"body":{"x":1},"method":"GET"}
    def f(filt, moul=posts, untrusted=False, prefix="", ld=loader, a=auth, c=ctx):
        return {"kind":"filter","filter":filt,"moul":moul,"auth":a,"context":c,"param_prefix":prefix,"untrusted":untrusted,"loader":ld}
    def s(sort, moul=posts, untrusted=False, ld=loader):
        return {"kind":"sort","filter":sort,"moul":moul,"untrusted":untrusted,"loader":ld}
    F = [
     "title = 'a' && (price > 1 || active = false)", "title = null", "null = title", "title != null", "null != title", "null = null",
     "'x' = null", "price > null", "null", "title", "price", "'x'", "5", "true", "title:lower = 'abc'", "title:length > 3",
     "title:isset = true", "title:isset", "title:upper = 'x'", "nope:lower = 'x'", "title ~ 'a%b'", "title !~ 5", "5 ~ title",
     "title ~ body", "title ?= 'x'", "title ?~ 'x'", "tags ?= 'x'", "price = 1.5", "price = 1.2.3", "price = -3",
     "price > 99999999999999999999", "price ~ 1.5", "price ~ 100000000.0", "@request.auth.id = id", "id = @request.auth.email", "id = auth.id",
     "id = @auth.id", "id = @request.auth.nested.id", "id = @request.auth.age", "id = @request.auth.flag", "id = @request.auth.obj",
     "id ~ @request.auth.obj", "id ~ @request.auth.big", "id = @request.auth.nul", "title = @request.query.q", "title = @request.query.missing", "price > @request.query.n",
     "title = @request.headers.x_h", "title = @request.body.x", "title = @request.method",
     "team.owner = 'u1'", "team.owner ~ 'u'", "team.owner = null", "team.owner != null", "team.owner > null", "team.owner = title", "team.owner = team",
     "team.nope = 1", "team.owner.x = 1", "editors.displayName = 'Al'", "editors.displayName = null", "author.displayName = 'x'",
     "team.owner = @request.auth.id", "team.owner = title:lower", "team.owner = title:isset", "title = team.owner",
     "@collection.teams.owner = id", "@collection.teams.owner ~ 'x'", "@collection.teams.owner = null", "@collection.teams.owner = @request.auth.id",
     "@collection.teams.owner = 'x' && @collection.teams.id = id", "@collection.teams:t.owner = 1", "@collection.teams.x.y = 1",
     "@collection.teams", "@collection.teams.owner", "@collection.teams.owner =", "@collection.teams.owner = title:lower", "@collection.teams.owner ?= 'x'",
     "ssn = 'x'", "'x' = ssn", "ssn != null", "ssn = null", "ssn ~ 'x'", "notes = 'x'", "SSN = 'x'", "ssn = 5", "ssn = title", "title = ssn",
     "ssn = 1.0", "ssn = @request.auth.big", "ssn:lower = 'x'",
     "(title = 'a'", "title = 'a')", "title = 'a' title = 'b'", "title =", "= 'a'", "&& title", "title = 'a' &&", "title = 'unterminated",
     "title ! 'a'", "// only comment", "   ", "title = 'a' // comment", "(title)", "((title = 'a'))", "title = 'a' AND price > 1 OR active = true",
     "title = 'a' and (price > 1 or active = true)", "passwordHash = 'x'", "email = 'x'", "tokenKey:length > 1", "weird.title = 'x'", "weird.title = null",
     "ghost.title = 'x'", "bad.title = 'x'", "loose.title = 'x'", "title = (price)", "title = ,", "TRUE = active", "active = FALSE", "title = NULL",
     "created = 'x'", "worker = 1", "title = @request.auth.id && price > 5 || team.owner ~ 'q'",
    ]
    cases = []
    for flt in F:
        cases.append(f(flt))
    for flt in ["team.owner = 'u1'", "parent.title ~ 'x'", "editors.email ~ 'x'", "editors.displayName = 'Al'", "author.age > 3",
                "passwordHash = 'x'", "email = 'x'", "tokenKey:length > 1", "@collection.teams.owner = id", "title ~ 'x' && team.owner = title",
                "ssn = 'x'", "parent.title = null", "parent.parent = 'x'", "team.owner = @request.auth.id"]:
        cases.append(f(flt, untrusted=True, prefix="f_"))
    cases += [f("email = 'x'", moul=users, untrusted=True), f("displayName = 'x'", moul=users, untrusted=True, prefix="u_"),
              f("team.owner = 'x'", ld=None), f("team.owner = 'x'", a=None, c=None), f("id = @request.auth.id", a=None, c=None),
              f("title = @request.query.q", c=None), f("title = @request.query.q", c={"query": None}), f("title = @request.query.q", c={"query": "str"})]
    S_ = ["title", "-title,+price", " title , -price ", ",,", "@random", "-@random", "title DESC", "nope", "team.owner", "-team.owner",
          "editors.displayName", "author.displayName", "author.email", "ghost.title", "weird.title", "title.x", "team.owner.x", "passwordHash",
          "email", "-created", "+", "-", "team.", ".owner", "bad.title", "loose.title", "editors.nope"]
    for x in S_:
        cases.append(s(x))
        cases.append(s(x, untrusted=True))
    cases += [s("team.owner", ld=None), s("email", moul=users, untrusted=True), s("email", moul=users), s("tokenKey", moul=users, untrusted=True)]
    return cases
