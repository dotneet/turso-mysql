package e2e;

import jakarta.persistence.Column;
import jakarta.persistence.Entity;
import jakarta.persistence.FetchType;
import jakarta.persistence.GeneratedValue;
import jakarta.persistence.GenerationType;
import jakarta.persistence.Id;
import jakarta.persistence.JoinColumn;
import jakarta.persistence.JoinTable;
import jakarta.persistence.LockModeType;
import jakarta.persistence.ManyToMany;
import jakarta.persistence.ManyToOne;
import jakarta.persistence.OneToMany;
import jakarta.persistence.Table;
import jakarta.persistence.Version;
import java.math.BigDecimal;
import java.time.LocalDateTime;
import java.util.ArrayList;
import java.util.LinkedHashSet;
import java.util.List;
import java.util.Map;
import java.util.Optional;
import java.util.Set;
import org.hibernate.annotations.CreationTimestamp;
import org.hibernate.annotations.JdbcTypeCode;
import org.hibernate.annotations.UpdateTimestamp;
import org.hibernate.type.SqlTypes;
import org.springframework.data.domain.Page;
import org.springframework.data.domain.Pageable;
import org.springframework.data.domain.Slice;
import org.springframework.data.jpa.repository.EntityGraph;
import org.springframework.data.jpa.repository.JpaRepository;
import org.springframework.data.jpa.repository.Lock;
import org.springframework.data.jpa.repository.Modifying;
import org.springframework.data.jpa.repository.Query;
import org.springframework.data.repository.query.Param;

/** The blog schema every harness app uses, as JPA entities and Spring Data repositories. */
public final class Blog {
  private Blog() {}

  @Entity(name = "User")
  @Table(name = "users")
  public static class User {
    @Id
    @GeneratedValue(strategy = GenerationType.IDENTITY)
    public Long id;

    @Column(nullable = false, unique = true, length = 191)
    public String email;

    @Column(nullable = false, length = 100)
    public String name;

    @Column(nullable = false, precision = 10, scale = 2)
    public BigDecimal balance = BigDecimal.ZERO;

    @Column(name = "is_active", nullable = false)
    public boolean active = true;

    @JdbcTypeCode(SqlTypes.JSON)
    public Map<String, Object> profile;

    @Version
    @Column(nullable = false)
    public Long version;

    @CreationTimestamp
    @Column(name = "created_at", nullable = false, updatable = false)
    public LocalDateTime createdAt;

    @UpdateTimestamp
    @Column(name = "updated_at", nullable = false)
    public LocalDateTime updatedAt;

    @OneToMany(mappedBy = "user")
    public List<Post> posts = new ArrayList<>();

    public User() {}

    public User(String email, String name, String balance, boolean active, Map<String, Object> profile) {
      this.email = email;
      this.name = name;
      this.balance = new BigDecimal(balance);
      this.active = active;
      this.profile = profile;
    }
  }

  @Entity(name = "Post")
  @Table(name = "posts")
  public static class Post {
    @Id
    @GeneratedValue(strategy = GenerationType.IDENTITY)
    public Long id;

    @ManyToOne(fetch = FetchType.LAZY, optional = false)
    @JoinColumn(name = "user_id", nullable = false)
    public User user;

    @Column(nullable = false, length = 200)
    public String title;

    @Column(columnDefinition = "text")
    public String body;

    @Column(name = "published_at")
    public LocalDateTime publishedAt;

    @Column(nullable = false)
    public int views;

    @ManyToMany
    @JoinTable(name = "post_tags", joinColumns = @JoinColumn(name = "post_id"), inverseJoinColumns = @JoinColumn(name = "tag_id"))
    public Set<Tag> tags = new LinkedHashSet<>();

    public Post() {}

    public Post(User user, String title, String body, LocalDateTime publishedAt, int views) {
      this.user = user;
      this.title = title;
      this.body = body;
      this.publishedAt = publishedAt;
      this.views = views;
    }
  }

  @Entity(name = "Tag")
  @Table(name = "tags")
  public static class Tag {
    @Id
    @GeneratedValue(strategy = GenerationType.IDENTITY)
    public Long id;

    @Column(nullable = false, unique = true, length = 100)
    public String name;

    public Tag() {}

    public Tag(String name) {
      this.name = name;
    }
  }

  public record ActiveGroup(boolean active, long count, BigDecimal total, Double average, LocalDateTime latest) {}

  public interface UserRepository extends JpaRepository<User, Long> {
    Optional<User> findByEmail(String email);

    @Lock(LockModeType.PESSIMISTIC_WRITE)
    @Query("select u from User u where u.email = :email")
    Optional<User> lockByEmail(@Param("email") String email);

    @Query("select u from User u where not exists (select 1 from Post p where p.user = u)")
    List<User> findWithoutPosts();

    @Query("select new e2e.Blog$ActiveGroup(u.active, count(u), sum(u.balance), avg(u.balance), max(u.createdAt))"
        + " from User u group by u.active having count(u) > :min order by u.active")
    List<ActiveGroup> groupByActive(@Param("min") long min);

    @Query(value = "SELECT * FROM users WHERE JSON_UNQUOTE(JSON_EXTRACT(profile, '$.city')) = :city", nativeQuery = true)
    List<User> findByCity(@Param("city") String city);

    @Query(value = "SELECT COUNT(*) FROM users WHERE profile->>'$.city' = ?1", nativeQuery = true)
    long countByCityArrow(String city);

    @Modifying
    @Query(value = "UPDATE users SET profile = JSON_SET(profile, '$.city', :city) WHERE email = :email", nativeQuery = true)
    int moveTo(@Param("email") String email, @Param("city") String city);

    @Modifying
    @Query(value = "INSERT INTO users (email, name, balance, is_active, version, created_at, updated_at)"
        + " VALUES (:email, :name, :balance, 1, 0, NOW(6), NOW(6)) AS new"
        + " ON DUPLICATE KEY UPDATE name = new.name, balance = new.balance, updated_at = NOW(6)", nativeQuery = true)
    int upsert(@Param("email") String email, @Param("name") String name, @Param("balance") BigDecimal balance);
  }

  public interface PostRepository extends JpaRepository<Post, Long> {
    @EntityGraph(attributePaths = {"user", "tags"})
    @Query("select distinct p from Post p order by p.id")
    List<Post> findAllWithUserAndTags();

    @Query("select p from Post p join p.tags t join fetch p.user u where t.name = :tag and u.active = true")
    List<Post> findActiveByTag(@Param("tag") String tag);

    List<Post> findByTagsName(String name);

    long countByUserEmail(String email);

    Slice<Post> findByViewsGreaterThanEqual(int views, Pageable pageable);

    @Query(value = "select p from Post p join fetch p.user", countQuery = "select count(p) from Post p")
    Page<Post> pageWithUser(Pageable pageable);

    @Modifying
    @Query("update Post p set p.views = p.views + 1 where p.views < :limit")
    int bumpViewsBelow(@Param("limit") int limit);

    @Query(value = "SELECT p.user_id AS userId, SUM(p.views) AS views FROM posts p GROUP BY p.user_id HAVING SUM(p.views) > :min",
        nativeQuery = true)
    List<Object[]> busyAuthors(@Param("min") int min);

    @Lock(LockModeType.PESSIMISTIC_READ)
    @Query("select p from Post p where p.views >= 0")
    List<Post> readShared();
  }

  public interface TagRepository extends JpaRepository<Tag, Long> {
    Optional<Tag> findByName(String name);

    long countByNameIn(List<String> names);
  }
}
