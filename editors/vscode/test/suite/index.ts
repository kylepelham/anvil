import * as path from "path";
import Mocha from "mocha";

/** Called by VS Code in the test instance: runs the suite ANVIL_TEST_SUITE names. */
export function run(): Promise<void> {
  const mocha = new Mocha({ ui: "tdd", color: false, timeout: 60000 });
  mocha.addFile(path.join(__dirname, `${process.env.ANVIL_TEST_SUITE}.test.js`));
  return new Promise((resolve, reject) => {
    mocha.run((failures) => (failures > 0 ? reject(new Error(`${failures} tests failed`)) : resolve()));
  });
}
