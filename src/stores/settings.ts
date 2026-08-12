import { defineStore } from 'pinia'
import { mirrorsApi, settingsApi } from '@/api/tauri'
import type {
  AppSettings,
  Mirror,
  MirrorInput,
  MirrorPatch,
  SettingsPatch
} from '@/types/api'

const DEFAULT_REGISTRY = 'registry-1.docker.io'

function defaultSettings(): AppSettings {
  return {
    defaultOutputDir: '',
    defaultArch: 'amd64',
    defaultMirror: DEFAULT_REGISTRY,
    downloadWorkers: 4,
    lastPackageName: null
  }
}

interface SettingsState {
  settings: AppSettings
  mirrors: Mirror[]
  loading: boolean
}

export const useSettingsStore = defineStore('settings', {
  state: (): SettingsState => ({
    settings: defaultSettings(),
    mirrors: [],
    loading: false
  }),

  actions: {
    async loadSettings(): Promise<void> {
      this.loading = true
      try {
        const [settings, mirrors] = await Promise.all([
          settingsApi.get(),
          mirrorsApi.list()
        ])
        this.settings = settings
        this.mirrors = mirrors
      } catch (error) {
        this.settings = defaultSettings()
        this.mirrors = []
        throw error
      } finally {
        this.loading = false
      }
    },

    async updateSettings(patch: SettingsPatch): Promise<void> {
      this.settings = await settingsApi.update(patch)
    },

    async addMirror(input: MirrorInput): Promise<void> {
      const mirror = await mirrorsApi.add(input)
      this.mirrors.push(mirror)
      if (mirror.isDefault) {
        this.markDefaultMirror(mirror.id)
        this.settings.defaultMirror = mirror.registry
      }
    },

    async updateMirror(id: string, patch: MirrorPatch): Promise<void> {
      const mirror = await mirrorsApi.update(id, patch)
      const index = this.mirrors.findIndex(item => item.id === id)
      if (index !== -1) {
        this.mirrors[index] = mirror
      }
      if (mirror.isDefault) {
        this.markDefaultMirror(mirror.id)
        this.settings.defaultMirror = mirror.registry
      }
    },

    async deleteMirror(id: string): Promise<void> {
      await mirrorsApi.delete(id)
      this.mirrors = this.mirrors.filter(mirror => mirror.id !== id)
    },

    async setDefaultMirror(id: string): Promise<void> {
      this.settings = await mirrorsApi.setDefault(id)
      this.markDefaultMirror(id)
    },

    markDefaultMirror(id: string): void {
      this.mirrors = this.mirrors.map(mirror => ({
        ...mirror,
        isDefault: mirror.id === id
      }))
    },

    getDefaultMirrorRegistry(): string {
      return this.getDefaultMirror()?.registry || this.settings.defaultMirror || DEFAULT_REGISTRY
    },

    getDefaultMirror(): Mirror | undefined {
      return this.mirrors.find(mirror => mirror.isDefault) || this.mirrors[0]
    },

    getMirrorByRegistry(registry: string): Mirror | undefined {
      return this.mirrors.find(mirror => mirror.registry === registry)
    },

    getMirrorCredentials(registry: string): { username: string | null; password: string | null } {
      const mirror = this.getMirrorByRegistry(registry)
      return {
        username: mirror?.username || null,
        password: mirror?.password || null
      }
    }
  }
})
