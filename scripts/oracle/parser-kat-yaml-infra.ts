/**
 * Known-answer cases for the YAML infra artifacts `yaml-parser.ts` takes refs
 * from (`infra-parser.ts`: Docker Compose, GitHub workflows, OpenAPI,
 * Kubernetes, plus other kinds a `.yaml` name can select). Imported by
 * `parser-kat.ts`; the answers come from WrongStack's own parser.
 */

type Case = { lang: string; file: string; content: string; source: string };

export function addYamlInfraCases(cases: Case[]): void {
  const Y = (name: string, file: string, content: string) =>
    cases.push({ lang: 'yaml', file, content, source: `synthetic:yaml-infra-${name}` });

  // ── Docker Compose ────────────────────────────────────────────────────────
  Y(
    'compose-basic',
    'C:\\kat\\app\\docker-compose.yml',
    'version: "3.9"\nservices:\n  web:\n    image: "nginx:1.25"\n    depends_on:\n      - db\n      - cache\n  db:\n    image: postgres:16\n  cache:\n    image: \'redis\'\nvolumes:\n  data:\n',
  );
  Y(
    'compose-four-space-map-depends',
    'C:\\kat\\compose.yaml',
    'services:\n    api:\n        build: .\n        depends_on:\n            db:\n                condition: service_healthy\n            queue:\n                condition: service_started\n        image: ghcr.io/x/api:latest # trailing\n    db:\n        image: postgres\n',
  );
  Y(
    'compose-crlf-comments',
    'C:\\kat\\docker-compose.yaml',
    '# top comment\r\nservices:\r\n  # svc comment\r\n  a:\r\n    depends_on: [b]\r\n    image: x/a\r\n\r\n  b:\r\n    image:   b@sha256:abc\r\nnetworks:\r\n  a:\r\n    image: not-a-service\r\n',
  );
  Y(
    'compose-weird',
    'C:\\kat\\x\\COMPOSE.YML',
    'services:\n  -bad: 1\n  ok.name_1:\n    depends_on:\n    - inline-list\n    - condition\n    environment:\n      - A=1\n  second:\n   odd-indent: 1\n   image: three\n  third:\n    image:\n',
  );
  Y('compose-empty-services', 'C:\\kat\\compose.yml', 'services:\n');
  Y('compose-no-services', 'C:\\kat\\compose.yml', 'x:\n  image: a\n');

  // ── GitHub workflows ──────────────────────────────────────────────────────
  Y(
    'gha-full',
    'C:\\kat\\repo\\.github\\workflows\\ci.yml',
    'name: CI\non:\n  push:\n    branches: [main]\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n      - name: Setup\n        uses: "dtolnay/rust-toolchain@stable"\n      - run: cargo test\n  test:\n    needs: build\n    runs-on: windows-latest\n    steps:\n      - uses: \'./.github/actions/local\'\n  deploy:\n    needs: [build, "test", \'lint\' ]\n    uses: org/repo/.github/workflows/deploy.yml@main\n',
  );
  Y(
    'gha-edge',
    'C:\\kat\\.github\\workflows\\x.yaml',
    'jobs:\n    a:\n        needs: [ ]\n        steps:\n            - uses: docker://alpine:3\n    b:\n        needs: a   \n        needs: a b\n        needs:\n            - a\n    c.d-e_f:\n        if: x\n#comment\nenv:\n  uses: not/ajob\n',
  );
  Y(
    'gha-quotes',
    'C:\\kat\\.github\\workflows\\q.yml',
    'jobs:\n  q:\n    needs: ["\'", \'"\', "", "x\'"]\n    steps:\n      -   uses:   a/b@v1\n',
  );
  Y('gha-not-workflows-dir', 'C:\\kat\\.github\\ci.yml', 'jobs:\n  a:\n    steps:\n      - uses: actions/checkout@v4\n');

  // ── OpenAPI ───────────────────────────────────────────────────────────────
  Y(
    'openapi-basic',
    'C:\\kat\\api\\openapi.yaml',
    'openapi: 3.1.0\npaths:\n  /users:\n    get:\n      responses:\n        "200":\n          content:\n            application/json:\n              schema:\n                $ref: "#/components/schemas/User"\n  "/users/{id}":\n    get: {}\ncomponents:\n  schemas:\n    User:\n      type: object\n      properties:\n        pet:\n          $ref: \'#/components/schemas/Pet\'\n    Pet:\n      type: object\n    _Private.v2:\n      type: string\n  responses:\n    NotFound: {}\n',
  );
  Y(
    'openapi-swagger-indent',
    'C:\\kat\\swagger.yml',
    'swagger: "2.0"\npaths:\n    /a/b-c:\n        post:\n            parameters:\n                - $ref: "#/components/schemas/Body"\ncomponents:\n    schemas:\n        Body:\n            type: object\n          Odd:\n            x: 1\nschemas:\n  Top:\n    a: 1\n  /not-a-path-in-schemas: 1\n',
  );
  Y(
    'openapi-refs-inline',
    'C:\\kat\\openapi.yml',
    'x: { $ref: "#/components/schemas/Inline" }\nitems: [{$ref: \'#/components/schemas/A\'}, {$ref: \'#/components/schemas/B\'}]\n/path: 1\n\'/quoted\': 2\n',
  );

  // ── Kubernetes ────────────────────────────────────────────────────────────
  Y(
    'k8s-multi-doc',
    'C:\\kat\\deploy\\k8s\\app.yaml',
    'apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  labels:\n    app: web\n  name: web\nspec:\n  template:\n    spec:\n      containers:\n        - name: web\n          image: "ghcr.io/x/web:1.2"\n        - image: busybox\n      volumes:\n        - persistentVolumeClaim:\n            claimName: data\n        - configMap:\n            configMapName: cfg\n---\napiVersion: v1\nkind: Service\nmetadata:\n  name: \'web-svc\'\nspec:\n  backend:\n    serviceName: web\n    secretName: tls\n',
  );
  Y(
    'k8s-by-content',
    'C:\\kat\\any\\thing.yml',
    'apiVersion: v1\nkind: ConfigMap\nmetadata:\n    namespace: x\n    name: cfg\n',
  );
  Y(
    'k8s-metadata-first',
    'C:\\kat\\manifests\\m.yaml',
    'metadata:\n  name: before-kind\nkind: Pod\nmetadata:\n  name: after\n  extra:\n    name: nested\n  ---\nkind: Job\nmetadata:\n    image: odd\n',
  );
  Y('k8s-suffix', 'C:\\kat\\a.k8s.YAML', 'kind: Secret\nmetadata:\n  name: s\n');
  Y('k8s-kubernetes-dir', 'C:\\kat\\kubernetes\\x.yml', '---\nkind: "Namespace"\nmetadata:\n  name: ns\n');
  Y('k8s-api-not-at-col0', 'C:\\kat\\plain.yaml', ' apiVersion: v1\nkind: Pod\nmetadata:\n  name: p\n');

  // ── Other kinds a .yaml name selects ──────────────────────────────────────
  Y('dockerfile-yaml', 'C:\\kat\\Dockerfile.yaml', 'FROM rust:1 AS build\nCOPY --from=build /a /b\n');
  Y('dotenv-yaml', 'C:\\kat\\.env.yaml', 'A=1\nB=${A}\n');

  // ── Whitespace edge cases ─────────────────────────────────────────────────
  Y('compose-bom', 'C:\\kat\\bom\\compose.yml', '\uFEFFservices:\n  a:\n    image: x\n');
  Y('compose-tabs-nbsp', 'C:\\kat\\tabs\\compose.yml', 'services:\n\ta:\n\t\timage: t\n\u00A0b:\n\u00A0\u00A0image: n\n');
  Y('gha-line-seps', 'C:\\kat\\.github\\workflows\\ls.yml', 'jobs:\n  a:\u2028    uses: x/y@1\n  b:\r    needs: a\n');
}
