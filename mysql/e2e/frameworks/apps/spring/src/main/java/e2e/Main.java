package e2e;

import com.fasterxml.jackson.databind.ObjectMapper;
import java.io.FileWriter;
import java.io.PrintWriter;
import java.io.StringWriter;
import java.nio.charset.StandardCharsets;
import java.util.LinkedHashMap;
import java.util.Map;
import org.springframework.boot.autoconfigure.SpringBootApplication;
import org.springframework.data.jpa.repository.config.EnableJpaRepositories;

/**
 * Spring Boot 3.5 with Hibernate 6, Spring Data JPA, Flyway and MySQL
 * Connector/J, run once per prepared-statement mode: `client-prep` (the
 * Connector/J default) or `server-prep` (useServerPrepStmts=true). Both use
 * rewriteBatchedStatements=true and cachePrepStmts=true, as tuned apps do.
 */
@SpringBootApplication
@EnableJpaRepositories(considerNestedRepositories = true)
public class Main {
  public static void main(String[] args) {
    String mode = args[0];
    String database = args[1];
    String url = "jdbc:mysql://" + System.getenv("E2E_HOST") + ":" + System.getenv("E2E_PORT") + "/" + database
        + "?sslMode=VERIFY_IDENTITY"
        + "&trustCertificateKeyStoreUrl=file:/tmp/ca.p12&trustCertificateKeyStoreType=PKCS12"
        + "&trustCertificateKeyStorePassword=changeit"
        + "&useServerPrepStmts=" + mode.equals("server-prep")
        + "&cachePrepStmts=true&rewriteBatchedStatements=true";
    new Flow(new Steps(mode), url, System.getenv("E2E_USER"), System.getenv("E2E_PASSWORD")).run();
    System.exit(0);
  }

  /** Appends {"step","ok","error"} lines to $E2E_OUT/steps.jsonl; a failing step never stops the run. */
  static final class Steps {
    private final String prefix;
    private final ObjectMapper json = new ObjectMapper();

    Steps(String prefix) {
      this.prefix = prefix;
    }

    interface Body {
      void run() throws Exception;
    }

    boolean run(String name, Body body) {
      String step = prefix + "/" + name;
      System.out.println("=== step " + step);
      Map<String, Object> entry = new LinkedHashMap<>();
      entry.put("step", step);
      try {
        body.run();
        entry.put("ok", true);
      } catch (Throwable e) {
        StringWriter trace = new StringWriter();
        e.printStackTrace(new PrintWriter(trace));
        String text = trace.toString();
        System.out.println(text);
        entry.put("ok", false);
        entry.put("error", text.substring(Math.max(0, text.length() - 2000)));
      }
      try (FileWriter out = new FileWriter(System.getenv("E2E_OUT") + "/steps.jsonl", StandardCharsets.UTF_8, true)) {
        out.write(json.writeValueAsString(entry) + "\n");
      } catch (Exception e) {
        throw new IllegalStateException(e);
      }
      return Boolean.TRUE.equals(entry.get("ok"));
    }
  }
}
