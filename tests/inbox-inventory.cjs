// Validate the production inventory fragments against GitHub's relevant types.
// Supply graphql 16 through NODE_PATH; no application dependency is added here.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const {buildSchema, parse, validate} = require('graphql');

const source = fs.readFileSync('crates/board-forge/src/pass/needs/inventory.rs', 'utf8');
const fields = source.match(/const FIELDS: &str = r#"([\s\S]*?)"#;/)?.[1];
assert.ok(fields, 'inventory fragments must be read from the production query');

// A minimal schema slice, preserving GitHub's distinct non-null enum types.
// Even exclusive union fragments must have compatible response shapes.
const schema = buildSchema(`
  enum IssueState { OPEN CLOSED }
  enum PullRequestState { OPEN CLOSED MERGED }
  type Label { name: String! }
  type LabelConnection { nodes: [Label] totalCount: Int! }
  type IssueConnection { nodes: [Issue] totalCount: Int! }
  type Issue {
    number: Int!
    repository: Repository!
    state: IssueState!
    labels(first: Int): LabelConnection
    parent: Issue
  }
  type PullRequest {
    state: PullRequestState!
    labels(first: Int): LabelConnection
    closingIssuesReferences(first: Int): IssueConnection!
  }
  union IssueOrPullRequest = Issue | PullRequest
  type Repository {
    nameWithOwner: String!
    issueOrPullRequest(number: Int!): IssueOrPullRequest
  }
  type Query { repository(owner: String!, name: String!): Repository }
`);
const query = (fragments) => parse(`
  query InboxInventory($o0: String!, $r0: String!, $n0: Int!) {
    i0: repository(owner: $o0, name: $r0) {
      issueOrPullRequest(number: $n0) { ${fragments} }
    }
  }
`);
assert.deepEqual(validate(schema, query(fields)), []);

// Prove that this schema-aware check detects the original production failure,
// rather than merely accepting whatever response a replay fabricates.
const unaliased = fields.replace(/\b(?:issueState|pullRequestState):\s*/g, '');
const errors = validate(schema, query(unaliased));
assert.ok(errors.some(({message}) => message.includes("Fields \"state\" conflict")
  && message.includes('IssueState!') && message.includes('PullRequestState!')),
  errors.map(({message}) => message).join('\n'));
console.log('Inventory union fragments validate; unaliased state fields are rejected.');
