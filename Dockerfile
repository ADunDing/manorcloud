# ManorCloud 的容器映像檔，由 .github/workflows/release.yml 在發佈時建置。
#
# 不在這裡編譯：直接使用 package.sh 已經編譯、檢查過 glibc 版本的程式，
# 映像檔裡的程式與 Release 的安裝包完全相同。release.yml 會先準備好：
#   docker/amd64/manorcloud   x86_64 的程式
#   docker/arm64/manorcloud   aarch64 的程式
#   docker/state/             空的資料夾，成為 /var/lib/manorcloud
#
# 沒有任何 RUN：建置 ARM 版時不需要模擬器。
#
# 基底是 distroless：只有 glibc、libgcc 與 CA 憑證（SSO 連線需要），沒有 shell 與套件管理程式。
# nonroot 版本以 UID 65532 執行：ManorCloud 本來就拒絕以 root 執行。
#
# 基底固定到摘要（@sha256:...）：每次建置用的都是同一份，Dependabot 也才看得出它有更新、開 PR
FROM gcr.io/distroless/cc-debian13:nonroot@sha256:532c7a94f9e96ba9db75eb9388f4227751131c7cb0a411d1a8521af4096135de

LABEL org.opencontainers.image.source="https://github.com/ADunDing/manorcloud" \
      org.opencontainers.image.description="Lightweight self-hosted cloud storage" \
      org.opencontainers.image.licenses="AGPL-3.0-or-later"

ARG TARGETARCH

# 資料庫與檔案都在 /var/lib/manorcloud。第一次使用 named volume 時，
# Docker 會把這個資料夾連同擁有者複製到 volume，ManorCloud 才能寫入
COPY --chown=65532:65532 docker/state/ /var/lib/manorcloud/
COPY --chmod=0755 docker/${TARGETARCH}/manorcloud /usr/local/bin/manorcloud

# MANORCLOUD_MANUAL_RUN：不是由 systemd 啟動也照樣執行（見 main.rs 的 check_run_allowed）。
# 容器內必須監聽所有介面；對外開放哪個埠由 docker run -p 決定
ENV MANORCLOUD_MANUAL_RUN=1 \
    MANORCLOUD_BIND=0.0.0.0:3000 \
    MANORCLOUD_DATA_DIR=/var/lib/manorcloud/data \
    MANORCLOUD_DATABASE_URL=sqlite:///var/lib/manorcloud/manorcloud.db

WORKDIR /var/lib/manorcloud
VOLUME /var/lib/manorcloud
EXPOSE 3000
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/manorcloud"]
CMD ["run"]
