/**
 * 解析用户输入的镜像包名，生成完整镜像地址
 *
 * 输入格式支持：
 * 1. nginx / nginx:latest -> 自动拼接默认镜像源的 library/nginx
 * 2. bitnami/nginx:1.25 -> 自动拼接默认镜像源的 bitnami/nginx
 * 3. registrylibrary.com.cn/lamboegg/ind-uc-ext-front:tag -> 使用用户指定的 registry
 * 4. registrylibrary.com.cn:5000/lamboegg/ind-uc-ext-front:tag -> 支持指定端口
 */

export interface ParsedImage {
  packageName: string
  repository: string
  fullImage: string
  tag: string
  registry: string
}

export function parseImageInput(input: string, mirrorRegistry: string = 'registry-1.docker.io'): ParsedImage | null {
  if (!input || !input.trim()) {
    return null
  }

  const trimmedInput = input.trim()

  // 检查是否包含 registry（地址中有 . 或 :端口号 且在第一个 / 之前）
  const firstSlashIndex = trimmedInput.indexOf('/')

  // 检测 registry 部分：第一个 / 之前，且包含 . 或 :数字（端口）
  const potentialRegistry = trimmedInput.substring(0, firstSlashIndex)
  const hasRegistry = firstSlashIndex > 0 && (
    potentialRegistry.includes('.') ||
    /:\d+$/.test(potentialRegistry)  // 匹配端口格式 :5000
  )

  let registry: string = mirrorRegistry
  let repository: string = ''
  let tag: string = 'latest'
  let remaining: string = trimmedInput

  if (hasRegistry) {
    // 用户输入了完整地址，提取 registry（可能包含端口）
    registry = potentialRegistry
    remaining = trimmedInput.substring(firstSlashIndex + 1)
  }

  // 分离 tag（: 后面是版本号，不在 registry 部分）
  const colonIndex = remaining.lastIndexOf(':')
  if (colonIndex > 0) {
    repository = remaining.substring(0, colonIndex)
    tag = remaining.substring(colonIndex + 1) || 'latest'
  } else {
    repository = remaining
    tag = 'latest'
  }

  // 如果 repository 不含 /，则添加 library/
  if (!repository.includes('/')) {
    repository = `library/${repository}`
  }

  const fullImage = `${registry}/${repository}:${tag}`
  const packageName = repository.split('/').pop() || repository

  return {
    packageName,
    repository,
    fullImage,
    tag,
    registry
  }
}
