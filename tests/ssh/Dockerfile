# SSH server for tunnel tests: user "tunnel" (password "tunnel") with TCP forwarding.
FROM alpine:3.20
RUN apk add --no-cache openssh \
 && ssh-keygen -A \
 && adduser -D -s /bin/sh tunnel \
 && echo "tunnel:tunnel" | chpasswd \
 && mkdir -p /home/tunnel/.ssh \
 && sed -i -E '/^[#[:space:]]*(AllowTcpForwarding|PasswordAuthentication|PubkeyAuthentication|StrictModes|AuthorizedKeysFile)/d' /etc/ssh/sshd_config \
 && printf '%s\n' \
      'AllowTcpForwarding yes' \
      'PasswordAuthentication yes' \
      'PubkeyAuthentication yes' \
      'StrictModes no' \
      'AuthorizedKeysFile /etc/ssh/authorized_keys/%u' \
      >> /etc/ssh/sshd_config
EXPOSE 22
CMD ["/usr/sbin/sshd", "-D", "-e"]
