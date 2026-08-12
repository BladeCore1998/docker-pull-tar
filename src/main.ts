import { createApp } from 'vue'
import { createPinia } from 'pinia'
import ElementPlus from 'element-plus'
import 'element-plus/dist/index.css'
import AppVue from './App.vue'
import router from './router'

const app = createApp(AppVue)
app.use(createPinia())
app.use(router)
app.use(ElementPlus)
app.mount('#app')